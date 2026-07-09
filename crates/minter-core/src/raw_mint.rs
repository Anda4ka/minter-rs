use alloy_primitives::{Address, U256};
use anyhow::{Context, Result};

use crate::abi::*;
use crate::gas;
use crate::rpc::RpcClient;
use crate::sign::*;
use crate::types::*;

use crate::types::Signer;

#[derive(Clone)]
pub struct RawMintConfig {
    pub contract: Address,
    pub function: String,
    pub params: Vec<String>,
    pub value: U256,
    pub gas: GasParams,
    pub dry_run: bool,
}

pub async fn discover_functions(
    rpc: &RpcClient,
    contract: &Address,
) -> Result<Vec<(String, String)>> {
    let bytecode = rpc
        .get_code(contract)
        .await
        .context("failed to get bytecode")?;
    if bytecode.is_empty() {
        anyhow::bail!("No bytecode at contract address. Is this a valid contract?");
    }
    let selectors = extract_selectors(&bytecode);
    let mut results = Vec::new();

    for sel in &selectors {
        let _hex_sel = hex::encode(sel);
        for (known_sel, sig) in KNOWN_MINT_SELECTORS {
            if known_sel == sel {
                results.push((sig.to_string(), "hardcoded".to_string()));
            }
        }
        let remote = lookup_4byte(sel).await;
        for sig in remote {
            if !results.iter().any(|(s, _)| s == &sig) {
                results.push((sig, "4byte".to_string()));
            }
        }
    }

    Ok(results)
}

pub async fn run_raw_mint(
    signers: &[Signer],
    rpc: &RpcClient,
    config: &RawMintConfig,
) -> Vec<MintResult> {
    let calldata = match build_calldata(&config.function, &config.params) {
        Ok(c) => c,
        Err(e) => {
            crate::rlog!("Failed to build calldata: {}", e);
            return signers
                .iter()
                .map(|s| MintResult {
                    address: s.address(),
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(e.to_string()),
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
                return vec![];
            }
        };
    let chain_id = rpc.chain_id().await.unwrap_or(1);

    crate::rlog!("\nSummary:");
    crate::rlog!("  Contract:  {:?}", config.contract);
    crate::rlog!("  Function:  {}", config.function);
    crate::rlog!("  Value:     {} wei", config.value);
    crate::rlog!(
        "  Gas:       max={}gwei priority={}gwei",
        max_fee / U256::from(1e9 as u64),
        max_priority_fee / U256::from(1e9 as u64)
    );
    crate::rlog!("  Wallets:   {}", signers.len());

    let mut handles = Vec::new();
    for signer in signers {
        let rpc = rpc.clone();
        let config = (*config).clone();
        let calldata = calldata.clone();
        let chain_id = chain_id;
        let max_fee = max_fee;
        let max_priority_fee = max_priority_fee;
        let gas_multiplier = config.gas.gas_multiplier;
        let signer = signer.clone();
        handles.push(tokio::spawn(async move {
            let addr = signer.address();
            crate::rlog!("\nWallet: {}", shorten_address(&addr));

            // Balance check
            if let Ok(balance) = rpc.balance(&addr).await {
                let min_needed = config.value + max_fee * U256::from(50000);
                if balance < min_needed {
                    crate::rlog!("  [WARN] Low balance: {} wei", balance);
                }
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
                        error: Some(format!("nonce: {}", e)),
                    };
                }
            };

            print!("  Simulating...");
            let gas_estimate = match rpc
                .estimate_gas(&addr, &config.contract, config.value, &calldata)
                .await
            {
                Ok(g) => {
                    crate::rlog!(" OK gas={}", g);
                    g
                }
                Err(e) => {
                    crate::rlog!(" FAILED: {}", e);
                    return MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("sim: {}", e)),
                    };
                }
            };
            let gas_limit = (gas_estimate as f64 * gas_multiplier) as u64;

            if config.dry_run {
                crate::rlog!("  DRY RUN OK");
                return MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::DryRunOk,
                    gas_used: Some(gas_estimate),
                    block_number: None,
                    error: None,
                };
            }

            let tx = BuiltTx {
                chain_id,
                nonce,
                to: config.contract,
                value: config.value,
                data: calldata,
                gas_limit,
                max_fee,
                max_priority_fee,
            };

            let (raw, _signed_hash) = match sign_transaction(&signer, &tx) {
                Ok((r, h)) => (r, h),
                Err(e) => {
                    return MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("sign: {}", e)),
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
                        error: Some(format!("send: {}", e)),
                    };
                }
            };

            print!("  Receipt...");
            match rpc.wait_for_receipt(&tx_hash, 120).await {
                Ok(receipt) => {
                    let info = crate::rpc::parse_receipt(&receipt);
                    if info.success {
                        crate::rlog!(" CONFIRMED block={} gas={}", info.block_number, info.gas_used);
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
                            error: Some("reverted".to_string()),
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
                        error: Some(format!("receipt: {}", e)),
                    }
                }
            }
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => crate::rlog!("Task panicked: {}", e),
        }
    }
    results
}
