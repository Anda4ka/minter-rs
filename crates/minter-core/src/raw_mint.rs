use alloy_primitives::{Address, U256};
use anyhow::{Context, Result};

use crate::abi::*;
use crate::flashbots::{self, BundleTx, FlashbotsClient, FlashbotsConfig, MAINNET_CHAIN_ID};
use crate::gas::{self, apply_gas_limit};
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
    /// When true, broadcast via Flashbots bundle (Ethereum mainnet only).
    pub use_flashbots: bool,
    pub flashbots: FlashbotsConfig,
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

    if config.use_flashbots && chain_id != MAINNET_CHAIN_ID {
        crate::rlog!(
            "Flashbots requires Ethereum mainnet (chainId 1), got {}",
            chain_id
        );
        return signers
            .iter()
            .map(|s| MintResult {
                address: s.address(),
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(format!(
                    "Flashbots only on Ethereum mainnet (chainId 1), RPC is {chain_id}"
                )),
            })
            .collect();
    }

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
    crate::rlog!(
        "  Broadcast: {}",
        if config.use_flashbots {
            "Flashbots bundle"
        } else {
            "public mempool"
        }
    );

    // —— Prepare: sim + sign per wallet (no broadcast yet if flashbots) ——
    let gas_multiplier = config.gas.gas_multiplier;
    let mut prepared: Vec<(Address, Option<BundleTx>, MintResult)> = Vec::new();

    for signer in signers {
        let addr = signer.address();
        crate::rlog!("\nWallet: {}", shorten_address(&addr));

        if let Ok(balance) = rpc.balance(&addr).await {
            let min_needed = config.value + max_fee * U256::from(50000);
            if balance < min_needed {
                crate::rlog!("  [WARN] Low balance: {} wei", balance);
            }
        }

        let nonce = match rpc.nonce(&addr).await {
            Ok(n) => n,
            Err(e) => {
                prepared.push((
                    addr,
                    None,
                    MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("nonce: {e}")),
                    },
                ));
                continue;
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
                prepared.push((
                    addr,
                    None,
                    MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("sim: {e}")),
                    },
                ));
                continue;
            }
        };
        // L2-safe limit (same helper as Disperse/Sweep)
        let gas_limit = apply_gas_limit(gas_estimate, gas_multiplier, chain_id, 21_000);

        if config.dry_run && !config.use_flashbots {
            crate::rlog!("  DRY RUN OK");
            prepared.push((
                addr,
                None,
                MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::DryRunOk,
                    gas_used: Some(gas_estimate),
                    block_number: None,
                    error: None,
                },
            ));
            continue;
        }

        let tx = BuiltTx {
            chain_id,
            nonce,
            to: config.contract,
            value: config.value,
            data: calldata.clone(),
            gas_limit,
            max_fee,
            max_priority_fee,
        };

        let (raw, signed_hash) = match sign_transaction(signer, &tx) {
            Ok((r, h)) => (r, h),
            Err(e) => {
                prepared.push((
                    addr,
                    None,
                    MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("sign: {e}")),
                    },
                ));
                continue;
            }
        };

        prepared.push((
            addr,
            Some(BundleTx {
                from: addr,
                raw,
                tx_hash: signed_hash,
            }),
            MintResult {
                address: addr,
                tx_hash: Some(signed_hash),
                status: WalletStatus::Sent,
                gas_used: Some(gas_estimate),
                block_number: None,
                error: None,
            },
        ));
    }

    // —— Flashbots dry: callBundle ——
    if config.use_flashbots && config.dry_run {
        let pieces: Vec<BundleTx> = prepared
            .iter()
            .filter_map(|(_, p, _)| p.clone())
            .collect();
        if pieces.is_empty() {
            return prepared.into_iter().map(|(_, _, r)| r).collect();
        }
        let auth = &signers[0];
        let client = match FlashbotsClient::new(config.flashbots.clone()) {
            Ok(c) => c,
            Err(e) => {
                return prepared
                    .into_iter()
                    .map(|(addr, _, _)| MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("flashbots client: {e}")),
                    })
                    .collect();
            }
        };
        let block = rpc.block_number().await.unwrap_or(0).saturating_add(1);
        crate::rlog!("Flashbots eth_callBundle @ block {}", block);
        match client.call_bundle(auth, &pieces, block).await {
            Ok(res) => {
                let errs = flashbots::call_bundle_errors(&res);
                crate::rlog!("  callBundle OK: {}", res);
                let mut out = Vec::new();
                let mut err_i = 0usize;
                for (addr, piece, _) in prepared {
                    if piece.is_none() {
                        // already failed earlier — find from original failures
                        out.push(MintResult {
                            address: addr,
                            tx_hash: None,
                            status: WalletStatus::Failed,
                            gas_used: None,
                            block_number: None,
                            error: Some("prep failed".into()),
                        });
                        continue;
                    }
                    let e = errs.get(err_i).and_then(|x| x.clone());
                    err_i += 1;
                    if let Some(err) = e {
                        out.push(MintResult {
                            address: addr,
                            tx_hash: None,
                            status: WalletStatus::Failed,
                            gas_used: None,
                            block_number: None,
                            error: Some(format!("sim FAIL (callBundle): {err}")),
                        });
                    } else {
                        out.push(MintResult {
                            address: addr,
                            tx_hash: None,
                            status: WalletStatus::DryRunOk,
                            gas_used: None,
                            block_number: None,
                            error: Some("sim OK (callBundle) — not submitted".into()),
                        });
                    }
                }
                return out;
            }
            Err(e) => {
                crate::rlog!("  callBundle FAIL: {}", e);
                return prepared
                    .into_iter()
                    .map(|(addr, piece, _)| MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(if piece.is_some() {
                            format!("callBundle: {e}")
                        } else {
                            "prep failed".into()
                        }),
                    })
                    .collect();
            }
        }
    }

    // —— Public live: send each ——
    if !config.use_flashbots {
        let mut results = Vec::new();
        for (addr, piece, row) in prepared {
            let Some(piece) = piece else {
                results.push(row);
                continue;
            };
            if config.dry_run {
                results.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::DryRunOk,
                    gas_used: row.gas_used,
                    block_number: None,
                    error: None,
                });
                continue;
            }
            print!("  Sending...");
            let tx_hash = match rpc.race_send(&piece.raw).await {
                Ok(h) => {
                    crate::rlog!(" OK {}", shorten_hash(&h));
                    h
                }
                Err(e) => {
                    crate::rlog!(" FAILED: {}", e);
                    results.push(MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("send: {e}")),
                    });
                    continue;
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
                        results.push(MintResult {
                            address: addr,
                            tx_hash: Some(tx_hash),
                            status: WalletStatus::Confirmed,
                            gas_used: Some(info.gas_used),
                            block_number: Some(info.block_number),
                            error: None,
                        });
                    } else {
                        crate::rlog!(" REVERTED block={}", info.block_number);
                        results.push(MintResult {
                            address: addr,
                            tx_hash: Some(tx_hash),
                            status: WalletStatus::Failed,
                            gas_used: Some(info.gas_used),
                            block_number: Some(info.block_number),
                            error: Some("reverted".to_string()),
                        });
                    }
                }
                Err(e) => {
                    crate::rlog!(" timeout: {}", e);
                    results.push(MintResult {
                        address: addr,
                        tx_hash: Some(tx_hash),
                        status: WalletStatus::Sent,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("receipt: {e}")),
                    });
                }
            }
            let _ = row;
        }
        return results;
    }

    // —— Flashbots live ——
    let pieces: Vec<BundleTx> = prepared
        .iter()
        .filter_map(|(_, p, _)| p.clone())
        .collect();
    if pieces.is_empty() {
        return prepared.into_iter().map(|(_, _, r)| r).collect();
    }

    let auth = &signers[0];
    let client = match FlashbotsClient::new(config.flashbots.clone()) {
        Ok(c) => c,
        Err(e) => {
            return prepared
                .into_iter()
                .map(|(addr, _, _)| MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("flashbots client: {e}")),
                })
                .collect();
        }
    };

    let current = rpc.block_number().await.unwrap_or(0);
    crate::rlog!(
        "Flashbots sendBundle window: current={} pieces={}",
        current,
        pieces.len()
    );
    match client
        .send_bundle_window(auth, &pieces, current, None)
        .await
    {
        Ok(sub) => {
            crate::rlog!(
                "  bundle submitted targets={:?} hash={:?}",
                sub.target_blocks,
                sub.bundle_hash
            );
        }
        Err(e) => {
            crate::rlog!("  sendBundle window failed: {}", e);
            return prepared
                .into_iter()
                .map(|(addr, piece, _)| MintResult {
                    address: addr,
                    tx_hash: piece.as_ref().map(|p| p.tx_hash),
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("flashbots send: {e}")),
                })
                .collect();
        }
    }

    let mut results = Vec::new();
    for (addr, piece, _) in prepared {
        let Some(piece) = piece else {
            results.push(MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some("prep failed".into()),
            });
            continue;
        };
        print!("  Receipt {}...", shorten_hash(&piece.tx_hash));
        match rpc.wait_for_receipt(&piece.tx_hash, 90).await {
            Ok(receipt) => {
                let info = crate::rpc::parse_receipt(&receipt);
                if info.success {
                    crate::rlog!(" CONFIRMED block={}", info.block_number);
                    results.push(MintResult {
                        address: addr,
                        tx_hash: Some(piece.tx_hash),
                        status: WalletStatus::Confirmed,
                        gas_used: Some(info.gas_used),
                        block_number: Some(info.block_number),
                        error: None,
                    });
                } else {
                    crate::rlog!(" REVERTED");
                    results.push(MintResult {
                        address: addr,
                        tx_hash: Some(piece.tx_hash),
                        status: WalletStatus::Failed,
                        gas_used: Some(info.gas_used),
                        block_number: Some(info.block_number),
                        error: Some("reverted".into()),
                    });
                }
            }
            Err(e) => {
                crate::rlog!(" not included / timeout: {}", e);
                results.push(MintResult {
                    address: addr,
                    tx_hash: Some(piece.tx_hash),
                    status: WalletStatus::Sent,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!(
                        "submitted — not included (receipt timeout: {e})"
                    )),
                });
            }
        }
    }
    results
}
