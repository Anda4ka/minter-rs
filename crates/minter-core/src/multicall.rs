//! One-wallet Multicall3 batch: several contract calls in a single tx.
//!
//! Uses canonical Multicall3 `aggregate3Value` so each call can carry native value.
//! Warning: target contracts see `msg.sender` = Multicall3 (not the EOA), unless they use `tx.origin`.

use alloy::sol;
use alloy::sol_types::SolCall;
use alloy_primitives::{Address, Bytes, U256};
use anyhow::{bail, Context};

use crate::abi::build_calldata;
use crate::amount;
use crate::gas::{self, resolve_call_gas};
use crate::rpc::RpcClient;
use crate::sign::{sign_transaction, shorten_address, shorten_hash, BuiltTx};
use crate::types::{GasParams, MintResult, Signer, WalletStatus};

/// Canonical Multicall3 deployment (same address on most EVM chains).
pub const MULTICALL3: Address = alloy_primitives::address!("0xcA11bde05977b3631167028862bE2a173976CA11");

sol! {
    /// https://github.com/mds1/multicall
    struct Call3Value {
        address target;
        bool allowFailure;
        uint256 value;
        bytes callData;
    }

    struct CallResult {
        bool success;
        bytes returnData;
    }

    function aggregate3Value(Call3Value[] calls) external payable returns (CallResult[] memory returnData);
}

#[derive(Debug, Clone)]
pub struct MulticallStep {
    pub target: Address,
    /// ETH value for this call (wei).
    pub value: U256,
    pub call_data: Bytes,
    /// If true, Multicall3 continues on revert (not recommended for mint).
    pub allow_failure: bool,
    /// Optional label for logs / UI.
    pub label: String,
}

#[derive(Clone)]
pub struct MulticallConfig {
    pub multicall: Address,
    pub steps: Vec<MulticallStep>,
    pub gas: GasParams,
    pub dry_run: bool,
}

/// Build a step from function signature + comma params (same as Raw Mint).
pub fn step_from_function(
    target: Address,
    function: &str,
    params: &[String],
    value_eth: &str,
    allow_failure: bool,
    label: impl Into<String>,
) -> anyhow::Result<MulticallStep> {
    let call_data = if function.trim().is_empty() {
        Bytes::new()
    } else {
        build_calldata(function.trim(), params)?
    };
    let value = if value_eth.trim().is_empty() || value_eth.trim() == "0" {
        U256::ZERO
    } else {
        amount::eth_to_wei(value_eth.trim()).context("invalid value ETH")?
    };
    Ok(MulticallStep {
        target,
        value,
        call_data,
        allow_failure,
        label: label.into(),
    })
}

/// Build step from raw hex calldata (`0x…`).
pub fn step_from_calldata(
    target: Address,
    data_hex: &str,
    value_eth: &str,
    allow_failure: bool,
    label: impl Into<String>,
) -> anyhow::Result<MulticallStep> {
    let t = data_hex.trim();
    let call_data = if t.is_empty() || t == "0x" {
        Bytes::new()
    } else {
        let hex = t.strip_prefix("0x").unwrap_or(t);
        let bytes = hex::decode(hex).context("invalid calldata hex")?;
        Bytes::from(bytes)
    };
    let value = if value_eth.trim().is_empty() || value_eth.trim() == "0" {
        U256::ZERO
    } else {
        amount::eth_to_wei(value_eth.trim()).context("invalid value ETH")?
    };
    Ok(MulticallStep {
        target,
        value,
        call_data,
        allow_failure,
        label: label.into(),
    })
}

pub fn encode_aggregate3_value(steps: &[MulticallStep]) -> anyhow::Result<(Bytes, U256)> {
    if steps.is_empty() {
        bail!("Multicall needs at least one call");
    }
    let mut total_value = U256::ZERO;
    let mut calls = Vec::with_capacity(steps.len());
    for s in steps {
        total_value = total_value.saturating_add(s.value);
        calls.push(Call3Value {
            target: s.target,
            allowFailure: s.allow_failure,
            value: s.value,
            callData: s.call_data.clone().into(),
        });
    }
    let data = aggregate3ValueCall { calls }.abi_encode();
    Ok((Bytes::from(data), total_value))
}

/// Run multicall from a single signer.
pub async fn run_multicall(
    from: &Signer,
    rpc: &RpcClient,
    config: &MulticallConfig,
) -> MintResult {
    let addr = from.address();
    let multicall = if config.multicall == Address::ZERO {
        MULTICALL3
    } else {
        config.multicall
    };

    let (calldata, total_value) = match encode_aggregate3_value(&config.steps) {
        Ok(v) => v,
        Err(e) => {
            return MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(e.to_string()),
            };
        }
    };

    crate::rlog!("\nMulticall Summary:");
    crate::rlog!("  From:      {}", shorten_address(&addr));
    crate::rlog!("  Helper:    {:?}", multicall);
    crate::rlog!("  Calls:     {}", config.steps.len());
    crate::rlog!("  Total ETH: {} wei", total_value);
    for (i, s) in config.steps.iter().enumerate() {
        crate::rlog!(
            "    [{}] {} → {:?} value={} data={}b fail_ok={}",
            i + 1,
            if s.label.is_empty() { "?" } else { &s.label },
            s.target,
            s.value,
            s.call_data.len(),
            s.allow_failure
        );
    }

    // Preflight: eth_call each target with from = EOA (detect onlyEOA / WL that Multicall3 will break)
    crate::rlog!("  EOA preflight (msg.sender = your wallet):");
    let mut eoa_notes = Vec::new();
    for (i, s) in config.steps.iter().enumerate() {
        let label = if s.label.is_empty() {
            format!("#{}", i + 1)
        } else {
            s.label.clone()
        };
        match rpc
            .eth_call(&addr, &s.target, &s.call_data)
            .await
        {
            // eth_call with value is not always supported; use estimate_gas as proxy when value>0
            Ok(_) => {
                crate::rlog!("    [{}] EOA call OK", label);
                eoa_notes.push(format!("{label}:EOA_ok"));
            }
            Err(e) => {
                // Retry via estimate_gas (includes value)
                match rpc
                    .estimate_gas(&addr, &s.target, s.value, &s.call_data)
                    .await
                {
                    Ok(_) => {
                        crate::rlog!("    [{}] EOA estimate OK", label);
                        eoa_notes.push(format!("{label}:EOA_ok"));
                    }
                    Err(e2) => {
                        crate::rlog!(
                            "    [{}] EOA FAIL (call may need EOA msg.sender): {} / {}",
                            label,
                            e,
                            e2
                        );
                        eoa_notes.push(format!("{label}:EOA_fail"));
                    }
                }
            }
        }
    }
    let eoa_all_ok = eoa_notes.iter().all(|n| n.ends_with(":EOA_ok"));
    if !eoa_all_ok {
        crate::rlog!(
            "  [WARN] Some calls fail as EOA — Multicall3 sets msg.sender=helper; mint/WL often reverts"
        );
    }

    // Ensure multicall code exists
    match rpc.get_code(&multicall).await {
        Ok(code) if code.is_empty() => {
            return MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(format!(
                    "No contract at multicall address {:?} on this chain — deploy Multicall3 or set override",
                    multicall
                )),
            };
        }
        Err(e) => {
            return MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(format!("get_code multicall: {e}")),
            };
        }
        _ => {}
    }

    let chain_id = match rpc.chain_id().await {
        Ok(id) => id,
        Err(e) => {
            return MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(format!("chain_id: {e}")),
            };
        }
    };

    let (base_fee, network_priority) = rpc
        .fee_history()
        .await
        .unwrap_or((U256::from(1_000_000_000u64), U256::from(1_000_000_000u64)));
    let (max_fee, max_priority_fee) =
        match gas::calculate_fees(&config.gas, base_fee, network_priority) {
            Ok(f) => f,
            Err(e) => {
                return MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("gas: {e}")),
                };
            }
        };

    print!("  Simulating multicall (msg.sender=Multicall3)...");
    let gas_limit = resolve_call_gas(
        rpc,
        &addr,
        &multicall,
        total_value,
        &calldata,
        chain_id,
        config.gas.gas_multiplier,
    )
    .await;
    // Explicit multicall sim for clear errors
    let mc_sim = rpc
        .estimate_gas(&addr, &multicall, total_value, &calldata)
        .await;
    match &mc_sim {
        Ok(g) => crate::rlog!(" OK gas={}", g),
        Err(e) => {
            crate::rlog!(" FAILED: {}", e);
            let mut err = format!("multicall sim: {e}");
            if eoa_all_ok {
                err.push_str(
                    " | EOA preflight OK but Multicall3 fails → contract likely rejects non-EOA msg.sender",
                );
            } else {
                err.push_str(&format!(
                    " | EOA preflight: {}",
                    eoa_notes.join(", ")
                ));
            }
            return MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(err),
            };
        }
    }
    let gas_estimate = mc_sim.unwrap_or(gas_limit);

    if config.dry_run {
        crate::rlog!("  DRY RUN OK (multicall not broadcast)");
        let note = if eoa_all_ok {
            "sim OK (EOA preflight OK; Multicall3 sim OK)"
        } else {
            "sim OK Multicall3 — but EOA preflight had failures (see log)"
        };
        return MintResult {
            address: addr,
            tx_hash: None,
            status: WalletStatus::DryRunOk,
            gas_used: Some(gas_estimate),
            block_number: None,
            error: Some(format!(
                "{note} | {}",
                eoa_notes.join(", ")
            )),
        };
    }

    let nonce = match rpc.nonce(&addr).await {
        Ok(n) => n,
        Err(e) => {
            return MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(format!("nonce: {e}")),
            };
        }
    };

    let tx = BuiltTx {
        chain_id,
        nonce,
        to: multicall,
        value: total_value,
        data: calldata,
        gas_limit,
        max_fee,
        max_priority_fee,
    };

    let (raw, _hash) = match sign_transaction(from, &tx) {
        Ok(v) => v,
        Err(e) => {
            return MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(format!("sign: {e}")),
            };
        }
    };

    print!("  Sending...");
    let tx_hash = match rpc.race_send(&raw).await {
        Ok(h) => {
            crate::rlog!(" OK {}", shorten_hash(&h));
            h
        }
        Err(e) => {
            crate::rlog!(" FAILED: {}", e);
            return MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(format!("send: {e}")),
            };
        }
    };

    print!("  Receipt...");
    match rpc.wait_for_receipt(&tx_hash, 120).await {
        Ok(receipt) => {
            let info = crate::rpc::parse_receipt(&receipt);
            if info.success {
                crate::rlog!(
                    " CONFIRMED block={} gas={}",
                    info.block_number,
                    info.gas_used
                );
                MintResult {
                    address: addr,
                    tx_hash: Some(tx_hash),
                    status: WalletStatus::Confirmed,
                    gas_used: Some(info.gas_used),
                    block_number: Some(info.block_number),
                    error: None,
                }
            } else {
                crate::rlog!(" REVERTED block={}", info.block_number);
                MintResult {
                    address: addr,
                    tx_hash: Some(tx_hash),
                    status: WalletStatus::Failed,
                    gas_used: Some(info.gas_used),
                    block_number: Some(info.block_number),
                    error: Some("reverted (one of the calls failed — msg.sender is Multicall3)".into()),
                }
            }
        }
        Err(e) => {
            crate::rlog!(" timeout: {}", e);
            MintResult {
                address: addr,
                tx_hash: Some(tx_hash),
                status: WalletStatus::Sent,
                gas_used: None,
                block_number: None,
                error: Some(format!("receipt: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_nonempty() {
        let step = MulticallStep {
            target: Address::ZERO,
            value: U256::ZERO,
            call_data: Bytes::from(vec![0x12, 0x34, 0x56, 0x78]),
            allow_failure: false,
            label: "t".into(),
        };
        let (data, val) = encode_aggregate3_value(&[step]).unwrap();
        assert!(data.len() > 4);
        assert_eq!(val, U256::ZERO);
        // selector aggregate3Value
        let sel = &data.as_ref()[..4];
        assert_eq!(hex::encode(sel), hex::encode(aggregate3ValueCall::SELECTOR));
    }

    #[test]
    fn multicall3_constant() {
        assert_eq!(
            format!("{:?}", MULTICALL3).to_lowercase(),
            "0xca11bde05977b3631167028862be2a173976ca11"
        );
    }
}
