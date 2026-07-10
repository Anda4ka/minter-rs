//! Disperse native coin: one vault wallet → many destinations (fixed amount each).

use alloy_primitives::{Address, Bytes, U256};
use anyhow::{bail, Context, Result};

use crate::gas;
use crate::rpc::RpcClient;
use crate::sign::*;
use crate::sweep::fmt_eth;
use crate::types::*;

use crate::types::Signer;

#[derive(Clone)]
pub struct DisperseConfig {
    pub amount: U256,
    pub gas: GasParams,
    pub dry_run: bool,
}

/// Send `amount` native coin from `from` to each `destinations` address (sequential nonces).
/// Each result row uses the **destination** address.
pub async fn run_disperse(
    from: &Signer,
    destinations: &[Address],
    rpc: &RpcClient,
    config: &DisperseConfig,
) -> Vec<MintResult> {
    if destinations.is_empty() {
        return vec![];
    }

    let from_addr = from.address();
    let chain_id = match rpc.chain_id().await {
        Ok(id) => id,
        Err(e) => {
            crate::rlog!("Failed to get chain ID: {}", e);
            return destinations
                .iter()
                .map(|d| MintResult {
                    address: *d,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("chain_id: {e}")),
                })
                .collect();
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
                crate::rlog!("Gas calculation failed: {}", e);
                return destinations
                    .iter()
                    .map(|d| MintResult {
                        address: *d,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("gas: {e}")),
                    })
                    .collect();
            }
        };

    // Native transfer gas: L2 / Orbit (Robinhood, Base, Arbitrum, …) often need
    // more than 21_000 intrinsic — always eth_estimateGas + safe floor.
    let sample_to = destinations
        .iter()
        .copied()
        .find(|d| *d != from_addr)
        .unwrap_or(destinations[0]);
    let gas_limit = resolve_native_transfer_gas(
        rpc,
        &from_addr,
        &sample_to,
        config.amount,
        chain_id,
        config.gas.gas_multiplier,
    )
    .await;
    let n = destinations.len() as u64;
    let gas_cost_one = max_fee * U256::from(gas_limit);
    let total_need = config.amount * U256::from(n) + gas_cost_one * U256::from(n);

    crate::rlog!("\nDisperse Summary:");
    crate::rlog!("  From:        {:?}", from_addr);
    crate::rlog!("  To:          {} wallet(s)", destinations.len());
    crate::rlog!("  Amount each: {} ETH", fmt_eth(config.amount));
    crate::rlog!("  Total value: {} ETH", fmt_eth(config.amount * U256::from(n)));
    crate::rlog!("  Chain ID:    {}", chain_id);
    crate::rlog!(
        "  Gas:         max={}gwei priority={}gwei limit={}",
        max_fee / U256::from(1_000_000_000u64),
        max_priority_fee / U256::from(1_000_000_000u64),
        gas_limit
    );
    if config.dry_run {
        crate::rlog!("  Mode:        DRY RUN");
    }

    let balance = match rpc.balance(&from_addr).await {
        Ok(b) => b,
        Err(e) => {
            crate::rlog!("  balance failed: {}", e);
            return destinations
                .iter()
                .map(|d| MintResult {
                    address: *d,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("from balance: {e}")),
                })
                .collect();
        }
    };
    crate::rlog!("  From balance: {} ETH", fmt_eth(balance));
    crate::rlog!("  Need (value+gas est): {} ETH", fmt_eth(total_need));

    if balance < total_need {
        crate::rlog!(
            "  [WARN] Balance may be insufficient (need ~{} ETH, have {})",
            fmt_eth(total_need),
            fmt_eth(balance)
        );
    }

    if config.amount.is_zero() {
        return destinations
            .iter()
            .map(|d| MintResult {
                address: *d,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some("amount must be > 0".into()),
            })
            .collect();
    }

    let mut nonce = match rpc.nonce(&from_addr).await {
        Ok(n) => n,
        Err(e) => {
            return destinations
                .iter()
                .map(|d| MintResult {
                    address: *d,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("nonce: {e}")),
                })
                .collect();
        }
    };

    let mut gas_limit = gas_limit;
    let mut results = Vec::with_capacity(destinations.len());

    for dest in destinations {
        if *dest == from_addr {
            crate::rlog!("\n[{}] skip (same as from)", shorten_address(dest));
            results.push(MintResult {
                address: *dest,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some("destination is source wallet".into()),
            });
            continue;
        }

        crate::rlog!(
            "\n→ {}  amount={} ETH  nonce={} gas={}",
            shorten_address(dest),
            fmt_eth(config.amount),
            nonce,
            gas_limit
        );

        if config.dry_run {
            // eth_call-style balance check is enough for dry path
            if balance < config.amount + gas_cost_one {
                results.push(MintResult {
                    address: *dest,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!(
                        "insufficient balance for amount+gas (have {})",
                        fmt_eth(balance)
                    )),
                });
            } else {
                crate::rlog!("  DRY RUN OK");
                results.push(MintResult {
                    address: *dest,
                    tx_hash: None,
                    status: WalletStatus::DryRunOk,
                    gas_used: Some(gas_limit),
                    block_number: None,
                    error: None,
                });
            }
            // still advance logical nonce for dry summary consistency
            nonce = nonce.saturating_add(1);
            continue;
        }

        let mut send_result = try_send_native(
            from,
            rpc,
            chain_id,
            nonce,
            *dest,
            config.amount,
            gas_limit,
            max_fee,
            max_priority_fee,
        )
        .await;

        // Retry once with bumped gas if RPC rejects intrinsic gas
        if let Err(ref e) = send_result {
            let msg = e.to_lowercase();
            if msg.contains("intrinsic gas too low") || msg.contains("gas too low") {
                let bumped = gas_limit
                    .saturating_mul(3)
                    .max(300_000)
                    .min(2_000_000);
                crate::rlog!(
                    "  intrinsic gas too low — retry with gas_limit {} → {}",
                    gas_limit,
                    bumped
                );
                gas_limit = bumped;
                send_result = try_send_native(
                    from,
                    rpc,
                    chain_id,
                    nonce,
                    *dest,
                    config.amount,
                    gas_limit,
                    max_fee,
                    max_priority_fee,
                )
                .await;
            }
        }

        let tx_hash = match send_result {
            Ok(h) => {
                crate::rlog!("  sent tx={}", shorten_hash(&h));
                h
            }
            Err(e) => {
                crate::rlog!("  send FAILED: {}", e);
                results.push(MintResult {
                    address: *dest,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("send: {e}")),
                });
                break;
            }
        };

        nonce = nonce.saturating_add(1);

        match rpc.wait_for_receipt(&tx_hash, 120).await {
            Ok(receipt) => {
                let info = crate::rpc::parse_receipt(&receipt);
                if info.success {
                    crate::rlog!(
                        "  CONFIRMED block={} gas={}",
                        info.block_number,
                        info.gas_used
                    );
                    results.push(MintResult {
                        address: *dest,
                        tx_hash: Some(tx_hash),
                        status: WalletStatus::Confirmed,
                        gas_used: Some(info.gas_used),
                        block_number: Some(info.block_number),
                        error: None,
                    });
                } else {
                    crate::rlog!("  REVERTED block={}", info.block_number);
                    results.push(MintResult {
                        address: *dest,
                        tx_hash: Some(tx_hash),
                        status: WalletStatus::Failed,
                        gas_used: Some(info.gas_used),
                        block_number: Some(info.block_number),
                        error: Some("reverted".into()),
                    });
                    break;
                }
            }
            Err(e) => {
                crate::rlog!("  receipt timeout: {}", e);
                results.push(MintResult {
                    address: *dest,
                    tx_hash: Some(tx_hash),
                    status: WalletStatus::Sent,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("receipt: {e}")),
                });
                // continue — nonce already used; next may still work if first landed
            }
        }
    }

    // Mark remaining destinations as skipped if we aborted early
    if results.len() < destinations.len() {
        for dest in destinations.iter().skip(results.len()) {
            results.push(MintResult {
                address: *dest,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some("skipped after prior failure".into()),
            });
        }
    }

    results
}

/// Parse a list of destination address strings; rejects empty / invalid.
pub fn parse_destinations(raw: &[String]) -> Result<Vec<Address>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for s in raw {
        let t = s.trim();
        if t.is_empty() {
            continue;
        }
        let addr: Address = t.parse().with_context(|| format!("invalid address: {t}"))?;
        let key = format!("{:?}", addr).to_lowercase();
        if seen.insert(key) {
            out.push(addr);
        }
    }
    if out.is_empty() {
        bail!("No destination wallets selected");
    }
    Ok(out)
}

/// L2 / Orbit chains where a bare 21k transfer often fails with "intrinsic gas too low".
fn chain_needs_elevated_transfer_gas(chain_id: u64) -> bool {
    matches!(
        chain_id,
        10 |           // Optimism
        8453 |         // Base
        42161 |        // Arbitrum One
        42170 |        // Arbitrum Nova
        81457 |        // Blast
        7777777 |      // Zora
        33139 |        // ApeChain
        360 |          // Shape
        4326 |         // MegaETH
        4663 |         // Robinhood Chain
        143 // Monad (EVM L1 but still safer elevated floor)
    )
}

/// eth_estimateGas for empty-data transfer + multiplier + L2 floor.
async fn resolve_native_transfer_gas(
    rpc: &RpcClient,
    from: &Address,
    to: &Address,
    value: U256,
    chain_id: u64,
    gas_multiplier: f64,
) -> u64 {
    const MIN_EOA: u64 = 21_000;
    // Safe floor for Orbit / OP-stack style chains when estimate is low or fails.
    const L2_FLOOR: u64 = 150_000;
    const FALLBACK: u64 = 200_000;

    let mult = if gas_multiplier > 1.0 {
        gas_multiplier
    } else {
        1.15
    };

    let estimated = match rpc
        .estimate_gas(from, to, value, &Bytes::new())
        .await
    {
        Ok(g) => {
            crate::rlog!("  eth_estimateGas(transfer) = {}", g);
            g.max(MIN_EOA)
        }
        Err(e) => {
            crate::rlog!("  eth_estimateGas failed ({e}) — using fallback {}", FALLBACK);
            FALLBACK
        }
    };

    let mut limit = ((estimated as f64) * mult).ceil() as u64;
    limit = limit.max(estimated).max(MIN_EOA);

    if chain_needs_elevated_transfer_gas(chain_id) {
        limit = limit.max(L2_FLOOR);
    }

    // Cap absurd estimates (still leave headroom for L2)
    limit.min(2_000_000)
}

async fn try_send_native(
    from: &Signer,
    rpc: &RpcClient,
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    gas_limit: u64,
    max_fee: U256,
    max_priority_fee: U256,
) -> Result<alloy_primitives::B256, String> {
    let tx = BuiltTx {
        chain_id,
        nonce,
        to,
        value,
        data: Bytes::new(),
        gas_limit,
        max_fee,
        max_priority_fee,
    };
    let (raw, _hash) =
        sign_transaction(from, &tx).map_err(|e| format!("sign: {e}"))?;
    rpc.race_send(&raw)
        .await
        .map_err(|e| e.to_string())
}
