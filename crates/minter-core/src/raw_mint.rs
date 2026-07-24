use alloy_primitives::{Address, U256};
use anyhow::{Context, Result};

use crate::abi::{
    EIP1967_IMPLEMENTATION_SLOT, KNOWN_MINT_SELECTORS, build_calldata, extract_selectors,
    is_mint_like_signature, lookup_4byte, parse_eip1167_implementation,
};
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
    /// Optional hard gas limit (skips estimate scaling when set).
    pub gas_limit: Option<u64>,
}

/// Resolve EIP-1167 / EIP-1967 proxy → implementation address (if any).
async fn resolve_implementation(
    rpc: &RpcClient,
    contract: &Address,
    bytecode: &[u8],
) -> Result<Option<(Address, &'static str)>> {
    if let Some(impl_addr) = parse_eip1167_implementation(bytecode) {
        if impl_addr != Address::ZERO && &impl_addr != contract {
            return Ok(Some((impl_addr, "eip1167")));
        }
    }
    // EIP-1967 implementation slot
    let slot = alloy_primitives::B256::from(EIP1967_IMPLEMENTATION_SLOT);
    if let Ok(word) = rpc.get_storage_at(contract, slot).await {
        let bytes = word.as_slice();
        if bytes.len() == 32 {
            let mut raw = [0u8; 20];
            raw.copy_from_slice(&bytes[12..32]);
            let impl_addr = Address::from(raw);
            if impl_addr != Address::ZERO && &impl_addr != contract {
                return Ok(Some((impl_addr, "eip1967")));
            }
        }
    }
    Ok(None)
}

/// Best-effort verified ABI from Blockscout-compatible explorers.
async fn fetch_explorer_abi_functions(
    explorer_base: &str,
    address: &Address,
) -> Vec<(String, String)> {
    let url = format!(
        "{}/api?module=contract&action=getabi&address={:?}",
        explorer_base.trim_end_matches('/'),
        address
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return vec![],
    };
    let data: serde_json::Value = match resp.json().await {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let result = data.get("result").and_then(|v| v.as_str()).unwrap_or("");
    if result.is_empty() || result == "Contract source code not verified" {
        return vec![];
    }
    let abi: Vec<serde_json::Value> = match serde_json::from_str(result) {
        Ok(a) => a,
        Err(_) => return vec![],
    };
    let mut out = Vec::new();
    for item in abi {
        if item.get("type").and_then(|t| t.as_str()) != Some("function") {
            continue;
        }
        let name = match item.get("name").and_then(|n| n.as_str()) {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let inputs = item
            .get("inputs")
            .and_then(|i| i.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|inp| inp.get("type").and_then(|t| t.as_str()).map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let sig = format!("{name}({})", inputs.join(","));
        out.push((sig, "explorer".to_string()));
    }
    out
}

fn explorer_api_for_chain(chain: Option<&str>) -> Option<&'static str> {
    match chain.map(str::to_ascii_lowercase).as_deref() {
        Some("robinhood" | "robinhood_chain" | "robinhood-chain") => {
            Some("https://robinhoodchain.blockscout.com")
        }
        Some("base") => Some("https://base.blockscout.com"),
        Some("ethereum" | "mainnet" | "eth") => Some("https://eth.blockscout.com"),
        Some("optimism" | "op") => Some("https://optimism.blockscout.com"),
        Some("arbitrum" | "arb") => Some("https://arbitrum.blockscout.com"),
        Some("polygon" | "matic") => Some("https://polygon.blockscout.com"),
        Some("zora") => Some("https://explorer.zora.energy"),
        Some("blast") => Some("https://blast.blockscout.com"),
        Some("shape") => Some("https://shapescan.xyz"),
        _ => None,
    }
}

/// Discover callable functions on a contract (for Raw Mint UI).
/// Resolves minimal/EIP-1967 proxies, then combines hardcoded + 4byte + explorer ABI.
pub async fn discover_functions(
    rpc: &RpcClient,
    contract: &Address,
) -> Result<Vec<(String, String)>> {
    discover_functions_on_chain(rpc, contract, None).await
}

pub async fn discover_functions_on_chain(
    rpc: &RpcClient,
    contract: &Address,
    chain: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let bytecode = rpc
        .get_code(contract)
        .await
        .context("failed to get bytecode")?;
    if bytecode.is_empty() {
        anyhow::bail!("No bytecode at contract address. Is this a valid contract on this network?");
    }

    let mut code_addr = *contract;
    let mut code = bytecode.to_vec();
    let mut proxy_note: Option<&'static str> = None;

    if let Some((impl_addr, kind)) = resolve_implementation(rpc, contract, &code).await? {
        let impl_code = rpc
            .get_code(&impl_addr)
            .await
            .with_context(|| format!("failed to get implementation bytecode {impl_addr:?}"))?;
        if impl_code.is_empty() {
            anyhow::bail!("Proxy ({kind}) points to {impl_addr:?} but implementation has no code");
        }
        code_addr = impl_addr;
        code = impl_code.to_vec();
        proxy_note = Some(kind);
        crate::rlog!(
            "Discover: {} proxy {:?} → impl {:?}",
            kind,
            contract,
            impl_addr
        );
    }

    let mut results: Vec<(String, String)> = Vec::new();
    let mut push_unique = |sig: String, source: String| {
        if !results.iter().any(|(s, _)| s == &sig) {
            results.push((sig, source));
        }
    };

    // 1) Verified explorer ABI (best source when contract is verified)
    if let Some(base) = explorer_api_for_chain(chain) {
        for (sig, src) in fetch_explorer_abi_functions(base, &code_addr).await {
            push_unique(sig, src);
        }
        // also try proxy address itself (some explorers verify proxy)
        if code_addr != *contract {
            for (sig, src) in fetch_explorer_abi_functions(base, contract).await {
                push_unique(sig, src);
            }
        }
    }

    // 2) Bytecode selectors → hardcoded mint + 4byte (parallel)
    let selectors = extract_selectors(&code);
    let mut pending_4byte: Vec<[u8; 4]> = Vec::new();
    for sel in &selectors {
        for (known_sel, sig) in KNOWN_MINT_SELECTORS {
            if *known_sel == sel.as_slice() {
                let src = match proxy_note {
                    Some(_) => "hardcoded@proxy",
                    None => "hardcoded",
                };
                push_unique(sig.to_string(), src.to_string());
            }
        }
        // skip 4byte if we already have mint-like from explorer
        pending_4byte.push(*sel);
    }

    // Cap remote lookups for speed (large ABIs can have 100+ PUSH4)
    const MAX_4BYTE: usize = 64;
    let to_lookup: Vec<[u8; 4]> = pending_4byte.into_iter().take(MAX_4BYTE).collect();
    let mut set = tokio::task::JoinSet::new();
    const CONC: usize = 10;
    let mut idx = 0;
    while idx < to_lookup.len() || !set.is_empty() {
        while set.len() < CONC && idx < to_lookup.len() {
            let sel = to_lookup[idx];
            idx += 1;
            set.spawn(async move { lookup_4byte(&sel).await });
        }
        if let Some(Ok(sigs)) = set.join_next().await {
            for sig in sigs {
                // Prefer mint-like; still keep other decoded names (useful for custom)
                let src = match proxy_note {
                    Some(_) => "4byte@proxy",
                    None => "4byte",
                };
                push_unique(sig, src.to_string());
            }
        }
    }

    // Rank: mint-like first, then rest (stable by signature)
    results.sort_by(|a, b| {
        let am = is_mint_like_signature(&a.0);
        let bm = is_mint_like_signature(&b.0);
        match (am, bm) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.cmp(&b.0),
        }
    });

    if results.is_empty() {
        if selectors.is_empty() {
            anyhow::bail!(
                "No function selectors in bytecode{}. Try pasting the function signature manually (e.g. mint(uint256)).",
                proxy_note
                    .map(|k| format!(" (resolved {k} proxy)"))
                    .unwrap_or_default()
            );
        }
        anyhow::bail!(
            "Found {} selector(s) in bytecode{} but none matched known mint functions or 4byte.directory. Enter the full signature manually in Function field (e.g. mint(uint256) or customName(uint256,address)).",
            selectors.len(),
            proxy_note
                .map(|k| format!(" via {k} proxy → {code_addr:?}"))
                .unwrap_or_default()
        );
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

    let (base_fee, network_priority) = match rpc.fee_history().await {
        Ok(f) => f,
        Err(e) => {
            crate::rlog!(
                "WARN fee_history failed ({e}) — falling back to 1 gwei base/priority; tx may be underpriced (audit M6)"
            );
            (U256::from(1_000_000_000u64), U256::from(1_000_000_000u64))
        }
    };
    let (max_fee, max_priority_fee) =
        match gas::calculate_fees(&config.gas, base_fee, network_priority) {
            Ok(f) => f,
            Err(e) => {
                crate::rlog!("Gas calculation failed: {}", e);
                return vec![];
            }
        };
    // Do not silently sign for mainnet if the configured RPC cannot report its
    // network. A fallback chain id can create transactions that are rejected or,
    // worse, interpreted on an unintended network.
    let chain_id = match rpc.chain_id().await {
        Ok(id) if id != 0 => id,
        Ok(_) => {
            let error = "RPC returned invalid chain id 0".to_string();
            return signers
                .iter()
                .map(|s| MintResult {
                    address: s.address(),
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(error.clone()),
                })
                .collect();
        }
        Err(e) => {
            let error = format!("chain id: {e}");
            return signers
                .iter()
                .map(|s| MintResult {
                    address: s.address(),
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(error.clone()),
                })
                .collect();
        }
    };

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
            let min_needed = config
                .value
                .saturating_add(max_fee.saturating_mul(U256::from(50000)));
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

        crate::rprint!("  Simulating...");
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
        // L2-safe limit (same helper as Disperse/Sweep), or hard override from UI
        let gas_limit = if let Some(gl) = config.gas_limit.filter(|&g| g >= 21_000) {
            gl
        } else {
            apply_gas_limit(gas_estimate, gas_multiplier, chain_id, 21_000)
        };

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
        let pieces: Vec<BundleTx> = prepared.iter().filter_map(|(_, p, _)| p.clone()).collect();
        if pieces.is_empty() {
            return prepared.into_iter().map(|(_, _, r)| r).collect();
        }
        let auth = &signers[0];
        let client = match FlashbotsClient::new(config.flashbots.clone(), chain_id) {
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
            crate::rprint!("  Sending...");
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
            crate::rprint!("  Receipt...");
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
    let pieces: Vec<BundleTx> = prepared.iter().filter_map(|(_, p, _)| p.clone()).collect();
    if pieces.is_empty() {
        return prepared.into_iter().map(|(_, _, r)| r).collect();
    }

    let auth = &signers[0];
    let client = match FlashbotsClient::new(config.flashbots.clone(), chain_id) {
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
        crate::rprint!("  Receipt {}...", shorten_hash(&piece.tx_hash));
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
                    error: Some(format!("submitted — not included (receipt timeout: {e})")),
                });
            }
        }
    }
    results
}
