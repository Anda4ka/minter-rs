//! Flashbots relay client (Ethereum mainnet bundles).
//!
//! Auth: `X-Flashbots-Signature: <addr>:<sig>` where sig is EIP-191 personal sign of
//! `keccak256(json_body)` (same as official Flashbots + ethers `signMessage(id(body))`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy::signers::SignerSync;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::types::Signer;

pub const DEFAULT_RELAY_URL: &str = "https://relay.flashbots.net";
pub const MAINNET_CHAIN_ID: u64 = 1;

#[derive(Debug, Clone)]
pub struct FlashbotsConfig {
    pub relay_url: String,
    /// How many next blocks to target (current+1 .. current+max_blocks).
    pub max_blocks: u64,
    /// Delay between resubmits to successive target blocks.
    pub resubmit_ms: u64,
}

impl Default for FlashbotsConfig {
    fn default() -> Self {
        Self {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            max_blocks: 3,
            resubmit_ms: 1200,
        }
    }
}

impl FlashbotsConfig {
    pub fn from_env(env: &std::collections::HashMap<String, String>) -> Self {
        let mut c = Self::default();
        if let Some(u) = env
            .get("FLASHBOTS_RELAY_URL")
            .or_else(|| env.get("FLASHBOTS_RELAY"))
        {
            let t = u.trim();
            if !t.is_empty() {
                c.relay_url = t.to_string();
            }
        }
        if let Some(n) = env
            .get("FLASHBOTS_MAX_BLOCKS")
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            if n > 0 {
                c.max_blocks = n.min(20);
            }
        }
        if let Some(ms) = env
            .get("FLASHBOTS_RESUBMIT_MS")
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            if ms >= 200 {
                c.resubmit_ms = ms;
            }
        }
        c
    }
}

#[derive(Debug, Clone)]
pub struct BundleTx {
    pub from: Address,
    pub raw: Bytes,
    /// Precomputed signed tx hash (for receipt polling).
    pub tx_hash: B256,
}

#[derive(Debug, Clone)]
pub struct BundleSubmitResult {
    /// Last relay response body (truncated).
    pub relay_response: String,
    /// Target blocks we submitted to.
    pub target_blocks: Vec<u64>,
    pub bundle_hash: Option<String>,
}

pub struct FlashbotsClient {
    http: reqwest::Client,
    config: FlashbotsConfig,
}

impl FlashbotsClient {
    pub fn new(config: FlashbotsConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("flashbots http client")?;
        Ok(Self { http, config })
    }

    pub fn config(&self) -> &FlashbotsConfig {
        &self.config
    }

    /// Sign Flashbots auth header for a JSON body string.
    ///
    /// The relay expects `signMessage(id(body))` semantics: EIP-191 personal-sign
    /// over the UTF-8 bytes of the **hex string** `0x<keccak256(body)>` — not over
    /// the raw 32 hash bytes (that variant is rejected as signature mismatch).
    pub fn sign_auth_header(auth: &Signer, body: &str) -> Result<String> {
        let id = keccak256(body.as_bytes());
        let id_hex = format!("0x{}", hex::encode(id));
        let sig = auth
            .sign_message_sync(id_hex.as_bytes())
            .context("flashbots auth sign")?;
        let addr = auth.address();
        let sig_hex = format!("0x{}", hex::encode(sig.as_bytes()));
        Ok(format!("{:?}:{}", addr, sig_hex))
    }

    async fn rpc(
        &self,
        auth: &Signer,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let body_obj = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let body = serde_json::to_string(&body_obj).context("flashbots serialize")?;
        let header = Self::sign_auth_header(auth, &body)?;
        let url = self.config.relay_url.trim_end_matches('/');

        let resp = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .header("X-Flashbots-Signature", header)
            .body(body)
            .send()
            .await
            .with_context(|| format!("flashbots POST {url}"))?;

        let status = resp.status();
        let text = resp.text().await.context("flashbots read body")?;
        if !status.is_success() {
            bail!(
                "Flashbots HTTP {status}: {}",
                crate::truncate_str(&text, 400)
            );
        }
        let data: Value = serde_json::from_str(&text).context("flashbots json")?;
        if let Some(err) = data.get("error") {
            bail!("Flashbots RPC error: {err}");
        }
        data.get("result")
            .cloned()
            .context("flashbots: no result field")
    }

    /// Simulate bundle at `block_number` (hex or decimal u64).
    pub async fn call_bundle(
        &self,
        auth: &Signer,
        txs: &[BundleTx],
        block_number: u64,
    ) -> Result<Value> {
        if txs.is_empty() {
            bail!("call_bundle: empty txs");
        }
        let raws: Vec<String> = txs
            .iter()
            .map(|t| format!("0x{}", hex::encode(t.raw.as_ref())))
            .collect();
        let params = json!([{
            "txs": raws,
            "blockNumber": format!("0x{:x}", block_number),
            "stateBlockNumber": "latest",
        }]);
        self.rpc(auth, "eth_callBundle", params).await
    }

    /// Submit bundle targeting a single block.
    pub async fn send_bundle(
        &self,
        auth: &Signer,
        txs: &[BundleTx],
        block_number: u64,
    ) -> Result<Value> {
        if txs.is_empty() {
            bail!("send_bundle: empty txs");
        }
        let raws: Vec<String> = txs
            .iter()
            .map(|t| format!("0x{}", hex::encode(t.raw.as_ref())))
            .collect();
        let params = json!([{
            "txs": raws,
            "blockNumber": format!("0x{:x}", block_number),
        }]);
        self.rpc(auth, "eth_sendBundle", params).await
    }

    /// Submit to next `max_blocks` blocks (resubmit). Stops if `cancel` is set.
    pub async fn send_bundle_window(
        &self,
        auth: &Signer,
        txs: &[BundleTx],
        current_block: u64,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<BundleSubmitResult> {
        let n = self.config.max_blocks.max(1);
        let mut target_blocks = Vec::new();
        let mut last_resp = String::new();
        let mut bundle_hash = None;

        for i in 1..=n {
            if cancel
                .as_ref()
                .map(|c| c.load(Ordering::SeqCst))
                .unwrap_or(false)
            {
                bail!("Flashbots submit cancelled");
            }
            let target = current_block.saturating_add(i);
            target_blocks.push(target);
            crate::rlog!("Flashbots eth_sendBundle target block {}", target);
            match self.send_bundle(auth, txs, target).await {
                Ok(v) => {
                    last_resp = v.to_string();
                    if let Some(h) = v.get("bundleHash").and_then(|x| x.as_str()) {
                        bundle_hash = Some(h.to_string());
                    } else if let Some(h) = v.as_str() {
                        bundle_hash = Some(h.to_string());
                    }
                    crate::rlog!(
                        "  sendBundle OK block {} resp={}",
                        target,
                        crate::truncate_str(&last_resp, 120)
                    );
                }
                Err(e) => {
                    last_resp = e.to_string();
                    crate::rlog!("  sendBundle FAIL block {}: {}", target, e);
                }
            }
            if i < n {
                tokio::time::sleep(Duration::from_millis(self.config.resubmit_ms)).await;
            }
        }

        Ok(BundleSubmitResult {
            relay_response: last_resp.chars().take(500).collect(),
            target_blocks,
            bundle_hash,
        })
    }
}

/// Parse per-tx results from eth_callBundle (best-effort).
pub fn call_bundle_errors(result: &Value) -> Vec<Option<String>> {
    let mut out = Vec::new();
    let results = result
        .get("results")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    for r in results {
        if let Some(err) = r.get("error").and_then(|e| e.as_str()) {
            out.push(Some(err.to_string()));
        } else if let Some(err) = r.get("revert").and_then(|e| e.as_str()) {
            out.push(Some(format!("revert: {err}")));
        } else {
            out.push(None);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let c = FlashbotsConfig::default();
        assert!(c.relay_url.contains("flashbots"));
        assert!(c.max_blocks >= 1);
    }

    #[test]
    fn auth_header_format() {
        // ephemeral key
        let key_bytes = [7u8; 32];
        let signer = alloy::signers::local::LocalSigner::from_bytes(&key_bytes.into())
            .expect("signer");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"eth_sendBundle","params":[]}"#;
        let h = FlashbotsClient::sign_auth_header(&signer, body).expect("sign");
        assert!(h.contains(':'));
        let parts: Vec<_> = h.splitn(2, ':').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("0x"));
        assert!(parts[1].starts_with("0x"));
        assert!(parts[1].len() > 10);
    }

    /// Signature must recover to the auth address for the **hex-string** message
    /// (`0x<keccak(body)>` as UTF-8), matching ethers `signMessage(id(body))`.
    #[test]
    fn auth_header_signs_hex_string_of_body_hash() {
        let key_bytes = [9u8; 32];
        let signer = alloy::signers::local::LocalSigner::from_bytes(&key_bytes.into())
            .expect("signer");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"eth_sendBundle","params":[]}"#;
        let h = FlashbotsClient::sign_auth_header(&signer, body).expect("sign");
        let (addr_s, sig_s) = h.split_once(':').expect("format");
        let sig_bytes = hex::decode(sig_s.trim_start_matches("0x")).expect("sig hex");
        let sig = alloy::signers::Signature::try_from(sig_bytes.as_slice()).expect("sig parse");
        let id_hex = format!("0x{}", hex::encode(keccak256(body.as_bytes())));
        let recovered = sig
            .recover_address_from_msg(id_hex.as_bytes())
            .expect("recover");
        assert_eq!(format!("{:?}", recovered), addr_s);
        // Raw 32-byte hash message must NOT recover to the same address.
        let raw_recovered = sig
            .recover_address_from_msg(keccak256(body.as_bytes()).as_slice())
            .expect("recover raw");
        assert_ne!(format!("{:?}", raw_recovered), addr_s);
    }
}
