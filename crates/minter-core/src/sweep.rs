use alloy_primitives::{Address, Bytes, U256};
use anyhow::{bail, Context, Result};

use crate::abi::function_selector;
use crate::gas;
use crate::rpc::RpcClient;
use crate::sign::*;
use crate::types::*;

use crate::types::Signer;

const ERC721_INTERFACE_ID: [u8; 4] = [0x80, 0xac, 0x58, 0xcd];
const ERC721_ENUMERABLE_INTERFACE_ID: [u8; 4] = [0x78, 0x0e, 0x9d, 0x63];

fn encode_address(addr: &Address) -> Vec<u8> {
    let mut buf = vec![0u8; 32];
    buf[12..].copy_from_slice(addr.as_slice());
    buf
}

fn encode_u256(val: U256) -> Vec<u8> {
    val.to_be_bytes::<32>().to_vec()
}

fn encode_bytes4(interface_id: [u8; 4]) -> Vec<u8> {
    let mut buf = vec![0u8; 32];
    buf[..4].copy_from_slice(&interface_id);
    buf
}

fn decode_u256(data: &[u8]) -> Result<U256> {
    if data.len() < 32 {
        bail!("expected >= 32 bytes, got {}", data.len());
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&data[..32]);
    Ok(U256::from_be_bytes(buf))
}

fn parse_token_id_str(s: &str) -> Option<U256> {
    if let Some(hex) = s.strip_prefix("0x") {
        U256::from_str_radix(hex, 16).ok()
    } else {
        U256::from_str_radix(s, 10).ok()
    }
}

fn alchemy_nft_domain(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("eth-mainnet.g.alchemy.com"),
        8453 => Some("base-mainnet.g.alchemy.com"),
        137 => Some("polygon-mainnet.g.alchemy.com"),
        42161 => Some("arb-mainnet.g.alchemy.com"),
        10 => Some("opt-mainnet.g.alchemy.com"),
        _ => None,
    }
}

async fn supports_interface(
    rpc: &RpcClient,
    contract: &Address,
    interface_id: [u8; 4],
) -> Result<bool> {
    let mut data = function_selector("supportsInterface(bytes4)").to_vec();
    data.extend(encode_bytes4(interface_id));
    let result = rpc
        .eth_call(&Address::ZERO, contract, &Bytes::from(data))
        .await
        .context("supportsInterface eth_call failed")?;
    Ok(result.len() >= 32 && result[31] != 0)
}

async fn balance_of(rpc: &RpcClient, contract: &Address, owner: &Address) -> Result<U256> {
    let mut data = function_selector("balanceOf(address)").to_vec();
    data.extend(encode_address(owner));
    let result = rpc
        .eth_call(&Address::ZERO, contract, &Bytes::from(data))
        .await
        .context("balanceOf eth_call failed")?;
    decode_u256(&result)
}

async fn token_of_owner_by_index(
    rpc: &RpcClient,
    contract: &Address,
    owner: &Address,
    index: U256,
) -> Result<U256> {
    let mut data = function_selector("tokenOfOwnerByIndex(address,uint256)").to_vec();
    data.extend(encode_address(owner));
    data.extend(encode_u256(index));
    let result = rpc
        .eth_call(&Address::ZERO, contract, &Bytes::from(data))
        .await
        .context("tokenOfOwnerByIndex eth_call failed")?;
    decode_u256(&result)
}

fn build_safe_transfer_calldata(from: &Address, to: &Address, token_id: U256) -> Bytes {
    let mut data = function_selector("safeTransferFrom(address,address,uint256)").to_vec();
    data.extend(encode_address(from));
    data.extend(encode_address(to));
    data.extend(encode_u256(token_id));
    Bytes::from(data)
}

async fn fetch_token_ids_alchemy(
    api_key: &str,
    domain: &str,
    contract: &Address,
    owner: &Address,
) -> Result<Vec<U256>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let mut all_ids = Vec::new();
    let mut page_key: Option<String> = None;

    loop {
        let mut url = format!(
            "https://{}/nft/v2/{}/getNFTsForOwner?owner={:?}&contractAddresses%5B%5D={:?}&pageSize=100",
            domain, api_key, owner, contract
        );
        if let Some(ref pk) = page_key {
            url.push_str(&format!("&pageKey={}", pk));
        }

        let resp = client
            .get(&url)
            .send()
            .await
            .context("Alchemy NFT API request failed")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Alchemy NFT API {}: {}", status, &text[..text.len().min(300)]);
        }
        let data: serde_json::Value =
            serde_json::from_str(&text).context("failed to parse Alchemy NFT API response")?;

        if let Some(nfts) = data.get("ownedNfts").and_then(|v| v.as_array()) {
            for nft in nfts {
                if let Some(tid) = nft
                    .get("id")
                    .and_then(|v| v.get("tokenId"))
                    .and_then(|v| v.as_str())
                    .and_then(parse_token_id_str)
                {
                    all_ids.push(tid);
                } else if let Some(tid) = nft
                    .get("tokenId")
                    .and_then(|v| v.as_str())
                    .and_then(parse_token_id_str)
                {
                    all_ids.push(tid);
                }
            }
        }

        page_key = data
            .get("pageKey")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if page_key.is_none() {
            break;
        }
    }

    Ok(all_ids)
}

pub struct SweepConfig {
    pub contract: Address,
    pub destination: Address,
    pub gas: GasParams,
    pub dry_run: bool,
    pub alchemy_api_key: Option<String>,
}

pub async fn run_sweep(
    signers: &[Signer],
    rpc: &RpcClient,
    config: &SweepConfig,
) -> Vec<MintResult> {
    let chain_id = match rpc.chain_id().await {
        Ok(id) => id,
        Err(e) => {
            crate::rlog!("Failed to get chain ID: {}", e);
            return vec![];
        }
    };

    let is_erc721 = supports_interface(rpc, &config.contract, ERC721_INTERFACE_ID)
        .await
        .unwrap_or(true);
    let has_enumerable =
        supports_interface(rpc, &config.contract, ERC721_ENUMERABLE_INTERFACE_ID)
            .await
            .unwrap_or(false);

    if !is_erc721 {
        crate::rlog!("Warning: contract may not be ERC721, proceeding anyway");
    }

    let alchemy_domain = alchemy_nft_domain(chain_id);
    let can_use_alchemy = config.alchemy_api_key.is_some() && alchemy_domain.is_some();

    if !has_enumerable {
        if can_use_alchemy {
            crate::rlog!(
                "No ERC721Enumerable — will use Alchemy NFT API to discover token IDs"
            );
        } else {
            crate::rlog!(
                "Warning: no ERC721Enumerable and no Alchemy API key — cannot auto-discover token IDs"
            );
        }
    }

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

    crate::rlog!("\nSweep Summary:");
    crate::rlog!("  Contract:    {:?}", config.contract);
    crate::rlog!("  Destination: {:?}", config.destination);
    crate::rlog!("  Chain ID:    {}", chain_id);
    crate::rlog!("  Wallets:     {}", signers.len());
    crate::rlog!(
        "  Gas:         max={}gwei priority={}gwei",
        max_fee / U256::from(1_000_000_000u64),
        max_priority_fee / U256::from(1_000_000_000u64)
    );
    if config.dry_run {
        crate::rlog!("  Mode:        DRY RUN");
    }

    let mut results: Vec<MintResult> = Vec::new();

    for signer in signers {
        let addr = signer.address();

        if addr == config.destination {
            crate::rlog!("\n[{}] skip (is destination)", shorten_address(&addr));
            continue;
        }

        crate::rlog!("\n=== Wallet {} ===", shorten_address(&addr));

        let balance = match balance_of(rpc, &config.contract, &addr).await {
            Ok(b) => b,
            Err(e) => {
                crate::rlog!("  balanceOf failed: {}", e);
                results.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("balanceOf: {}", e)),
                });
                continue;
            }
        };

        if balance == U256::ZERO {
            crate::rlog!("  No NFTs owned");
            continue;
        }

        // Saturating conversion: a broken/malicious contract can return a huge
        // balanceOf which would panic `to::<u128>()`. Cap enumeration too.
        const MAX_ENUMERATE: u64 = 10_000;
        let count = u64::try_from(balance).unwrap_or(u64::MAX).min(MAX_ENUMERATE);
        crate::rlog!("  Owns {} NFT(s)", count);

        let mut token_ids: Vec<U256> = Vec::new();

        if has_enumerable {
            for i in 0..count {
                match token_of_owner_by_index(rpc, &config.contract, &addr, U256::from(i)).await {
                    Ok(token_id) => {
                        token_ids.push(token_id);
                    }
                    Err(e) => {
                        crate::rlog!("  tokenOfOwnerByIndex({}) failed: {}", i, e);
                    }
                }
            }
        } else if can_use_alchemy {
            let api_key = config.alchemy_api_key.as_ref().unwrap();
            let domain = alchemy_domain.unwrap();
            crate::rlog!("  Fetching token IDs via Alchemy NFT API...");
            match fetch_token_ids_alchemy(api_key, domain, &config.contract, &addr).await {
                Ok(ids) => {
                    for id in &ids {
                        token_ids.push(*id);
                    }
                    crate::rlog!("  Alchemy returned {} token(s)", token_ids.len());
                }
                Err(e) => {
                    crate::rlog!("  Alchemy NFT API failed: {}", e);
                    results.push(MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("alchemy: {}", e)),
                    });
                    continue;
                }
            }
        } else {
            crate::rlog!("  Cannot enumerate: no ERC721Enumerable and no Alchemy API key");
            continue;
        }

        if token_ids.is_empty() {
            crate::rlog!("  No token IDs discovered");
            continue;
        }

        crate::rlog!(
            "  Transferring {} NFT(s) to {:?}",
            token_ids.len(),
            config.destination
        );
        for (i, tid) in token_ids.iter().enumerate() {
            crate::rlog!("    [{}/{}] tokenId={}", i + 1, token_ids.len(), tid);
        }

        let mut nonce = match rpc.nonce(&addr).await {
            Ok(n) => n,
            Err(e) => {
                crate::rlog!("  nonce failed: {}", e);
                results.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("nonce: {}", e)),
                });
                continue;
            }
        };

        let gas_multiplier = config.gas.gas_multiplier;

        for (i, token_id) in token_ids.iter().enumerate() {
            let calldata = build_safe_transfer_calldata(&addr, &config.destination, *token_id);

            let gas_limit = match rpc
                .estimate_gas(&addr, &config.contract, U256::ZERO, &calldata)
                .await
            {
                Ok(g) => {
                    crate::rlog!(
                        "  [{}/{}] tokenId={} estimateGas={}",
                        i + 1,
                        token_ids.len(),
                        token_id,
                        g
                    );
                    (g as f64 * gas_multiplier) as u64
                }
                Err(e) => {
                    crate::rlog!(
                        "  [{}/{}] tokenId={} estimateGas FAILED: {}",
                        i + 1,
                        token_ids.len(),
                        token_id,
                        e
                    );
                    continue;
                }
            };

            if config.dry_run {
                crate::rlog!(
                    "  [{}/{}] tokenId={} DRY RUN OK",
                    i + 1,
                    token_ids.len(),
                    token_id
                );
                results.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::DryRunOk,
                    gas_used: Some(gas_limit),
                    block_number: None,
                    error: None,
                });
                continue;
            }

            let tx = BuiltTx {
                chain_id,
                nonce,
                to: config.contract,
                value: U256::ZERO,
                data: calldata,
                gas_limit,
                max_fee,
                max_priority_fee,
            };

            let (raw, _signed_hash) = match sign_transaction(signer, &tx) {
                Ok((r, h)) => (r, h),
                Err(e) => {
                    crate::rlog!(
                        "  [{}/{}] tokenId={} sign FAILED: {}",
                        i + 1,
                        token_ids.len(),
                        token_id,
                        e
                    );
                    continue;
                }
            };

            let tx_hash = match rpc.race_send(&raw).await {
                Ok(h) => {
                    crate::rlog!(
                        "  [{}/{}] tokenId={} sent tx={}",
                        i + 1,
                        token_ids.len(),
                        token_id,
                        shorten_hash(&h)
                    );
                    h
                }
                Err(e) => {
                    crate::rlog!(
                        "  [{}/{}] tokenId={} send FAILED: {}",
                        i + 1,
                        token_ids.len(),
                        token_id,
                        e
                    );
                    continue;
                }
            };

            match rpc.wait_for_receipt(&tx_hash, 120).await {
                Ok(receipt) => {
                    nonce += 1;
                    let info = crate::rpc::parse_receipt(&receipt);
                    if info.success {
                        crate::rlog!(
                            "  [{}/{}] tokenId={} CONFIRMED block={} gas={}",
                            i + 1,
                            token_ids.len(),
                            token_id,
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
                        crate::rlog!(
                            "  [{}/{}] tokenId={} REVERTED block={}",
                            i + 1,
                            token_ids.len(),
                            token_id,
                            info.block_number
                        );
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
                    crate::rlog!(
                        "  [{}/{}] tokenId={} receipt timeout: {}",
                        i + 1,
                        token_ids.len(),
                        token_id,
                        e
                    );
                    match rpc.nonce(&addr).await {
                        Ok(n) => {
                            crate::rlog!("  nonce re-fetched: {} (was {})", n, nonce);
                            nonce = n;
                        }
                        Err(ne) => {
                            crate::rlog!("  WARN: nonce re-fetch failed: {}, using {}+1", ne, nonce);
                            nonce += 1;
                        }
                    }
                    results.push(MintResult {
                        address: addr,
                        tx_hash: Some(tx_hash),
                        status: WalletStatus::Sent,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("receipt: {}", e)),
                    });
                }
            }
        }
    }

    results
}

pub struct SweepEthConfig {
    pub destination: Address,
    pub gas: GasParams,
    pub dry_run: bool,
}

/// Format wei as ETH (`"1.500000"`), not the broken `"1.0.123456"` style.
pub fn fmt_eth(wei: U256) -> String {
    let scale = U256::from(1_000_000_000_000_000_000u64); // 1e18
    let whole = wei / scale;
    let frac = (wei % scale).to::<u128>();
    // First 6 decimal places of ETH: frac_wei / 10^12
    let micros = frac / 1_000_000_000_000u128;
    format!("{}.{:06}", whole, micros)
}

pub async fn run_sweep_eth(
    signers: &[Signer],
    rpc: &RpcClient,
    config: &SweepEthConfig,
) -> Vec<MintResult> {
    if signers.is_empty() {
        crate::rlog!("Sweep ETH: no wallets selected");
        return vec![];
    }
    let chain_id = match rpc.chain_id().await {
        Ok(id) => id,
        Err(e) => {
            crate::rlog!("Failed to get chain ID: {}", e);
            return vec![];
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

    // Unified L2-safe transfer gas (estimate + floor). Sample first non-dest wallet.
    let sample_from = signers
        .iter()
        .map(|s| s.address())
        .find(|a| *a != config.destination)
        .unwrap_or(signers[0].address());
    let gas_limit = crate::gas::resolve_native_transfer_gas(
        rpc,
        &sample_from,
        &config.destination,
        U256::ZERO, // estimate transfer shape; amount varies per wallet
        chain_id,
        config.gas.gas_multiplier,
    )
    .await;
    let gas_cost = max_fee * U256::from(gas_limit);

    crate::rlog!("\nETH Sweep Summary:");
    crate::rlog!("  Destination: {:?}", config.destination);
    crate::rlog!("  Chain ID:    {}", chain_id);
    crate::rlog!("  Wallets:     {}", signers.len());
    crate::rlog!(
        "  Gas:         max={}gwei priority={}gwei ({} gas for simple transfer)",
        max_fee / U256::from(1_000_000_000u64),
        max_priority_fee / U256::from(1_000_000_000u64),
        gas_limit
    );
    if config.dry_run {
        crate::rlog!("  Mode:        DRY RUN");
    }

    let mut results: Vec<MintResult> = Vec::new();

    for signer in signers {
        let addr = signer.address();

        if addr == config.destination {
            crate::rlog!("\n[{}] skip (is destination)", shorten_address(&addr));
            continue;
        }

        crate::rlog!("\n=== Wallet {} ===", shorten_address(&addr));

        let balance = match rpc.balance(&addr).await {
            Ok(b) => b,
            Err(e) => {
                crate::rlog!("  balance failed: {}", e);
                results.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("balance: {}", e)),
                });
                continue;
            }
        };

        crate::rlog!("  Balance: {} ETH", fmt_eth(balance));

        if balance <= gas_cost {
            crate::rlog!(
                "  Skipping: balance {} <= gas cost {} ETH",
                fmt_eth(balance),
                fmt_eth(gas_cost)
            );
            continue;
        }

        let transfer_amount = balance - gas_cost;
        crate::rlog!(
            "  Transfer: {} ETH (minus gas {} ETH)",
            fmt_eth(transfer_amount),
            fmt_eth(gas_cost)
        );

        if config.dry_run {
            crate::rlog!("  DRY RUN OK");
            results.push(MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::DryRunOk,
                gas_used: Some(gas_limit),
                block_number: None,
                error: None,
            });
            continue;
        }

        let nonce = match rpc.nonce(&addr).await {
            Ok(n) => n,
            Err(e) => {
                crate::rlog!("  nonce failed: {}", e);
                results.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("nonce: {}", e)),
                });
                continue;
            }
        };

        let tx = BuiltTx {
            chain_id,
            nonce,
            to: config.destination,
            value: transfer_amount,
            data: Bytes::new(),
            gas_limit,
            max_fee,
            max_priority_fee,
        };

        let (raw, _signed_hash) = match sign_transaction(signer, &tx) {
            Ok((r, h)) => {
                crate::rlog!("  sign OK ({} bytes)", r.len());
                (r, h)
            }
            Err(e) => {
                crate::rlog!("  sign FAILED: {}", e);
                results.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("sign: {}", e)),
                });
                continue;
            }
        };

        let tx_hash = match rpc.race_send(&raw).await {
            Ok(h) => {
                crate::rlog!("  sent tx={}", shorten_hash(&h));
                h
            }
            Err(e) => {
                crate::rlog!("  send FAILED: {}", e);
                results.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("send: {}", e)),
                });
                continue;
            }
        };

        match rpc.wait_for_receipt(&tx_hash, 120).await {
            Ok(receipt) => {
                let info = crate::rpc::parse_receipt(&receipt);
                if info.success {
                    crate::rlog!(
                        "  CONFIRMED block={} gas={} (sent {} ETH)",
                        info.block_number,
                        info.gas_used,
                        fmt_eth(transfer_amount)
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
                    crate::rlog!("  REVERTED block={}", info.block_number);
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
                crate::rlog!("  receipt timeout: {}", e);
                results.push(MintResult {
                    address: addr,
                    tx_hash: Some(tx_hash),
                    status: WalletStatus::Sent,
                    gas_used: None,
                    block_number: None,
                    error: Some(format!("receipt: {}", e)),
                });
            }
        }
    }

    results
}

#[cfg(test)]
mod fmt_eth_tests {
    use super::*;

    #[test]
    fn fmt_eth_whole_and_fraction() {
        let one = U256::from(1_000_000_000_000_000_000u64);
        assert_eq!(fmt_eth(one), "1.000000");
        assert_eq!(fmt_eth(one + one / U256::from(2u64)), "1.500000");
        // 0.08 ETH
        assert_eq!(
            fmt_eth(U256::from(80_000_000_000_000_000u64)),
            "0.080000"
        );
        assert_eq!(fmt_eth(U256::ZERO), "0.000000");
    }

    #[test]
    fn fmt_eth_no_nested_dot() {
        let s = fmt_eth(U256::from(1_500_000_000_000_000_000u64));
        assert_eq!(s.matches('.').count(), 1);
        assert!(!s.contains("1.0."));
    }
}
