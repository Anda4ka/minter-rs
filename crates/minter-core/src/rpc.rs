use alloy_primitives::{Address, B256, U256, bytes::Bytes};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::time::Duration;

pub struct RpcClient {
    client: reqwest::Client,
    urls: Vec<String>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Clone for RpcClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            urls: self.urls.clone(),
            next_id: self.next_id.clone(),
        }
    }
}

impl RpcClient {
    pub fn new(urls: Vec<String>) -> Self {
        Self::new_with_proxy(urls, None).expect("failed to create HTTP client")
    }

    /// Build RPC client; optional HTTP/SOCKS proxy for all JSON-RPC calls.
    pub fn new_with_proxy(urls: Vec<String>, proxy_url: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
        if let Some(p) = proxy_url.map(str::trim).filter(|s| !s.is_empty()) {
            let proxy = reqwest::Proxy::all(p).with_context(|| format!("invalid RPC proxy {p}"))?;
            builder = builder.proxy(proxy);
        }
        let client = builder.build().context("build RPC HTTP client")?;
        Ok(Self {
            client,
            urls,
            next_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        })
    }

    fn short_url(url: &str) -> String {
        // Char-based (not byte slicing): non-ASCII URLs must not panic.
        let chars: Vec<char> = url.chars().collect();
        if chars.len() > 42 {
            let head: String = chars[..30].iter().collect();
            let tail: String = chars[chars.len() - 8..].iter().collect();
            format!("{}...{}", head, tail)
        } else {
            url.to_string()
        }
    }

    async fn rpc_call_with_client(
        client: reqwest::Client,
        url: String,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let short = Self::short_url(&url);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("RPC {method} request failed via {short}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .with_context(|| format!("RPC {method}: failed to read response from {short}"))?;
        if !status.is_success() {
            bail!(
                "RPC {method} HTTP {status} via {short}: {}",
                crate::truncate_str(&text, 240)
            );
        }
        let data: serde_json::Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "RPC {method}: bad JSON from {short}: {}",
                crate::truncate_str(&text, 240)
            )
        })?;
        if let Some(error) = data.get("error") {
            bail!("RPC {method} via {short} error: {error}");
        }
        data.get("result")
            .cloned()
            .with_context(|| format!("RPC {method} via {short}: no result"))
    }

    pub async fn get_fastest_provider(&self) -> Result<String> {
        if self.urls.is_empty() {
            bail!("No RPC URLs configured");
        }

        crate::rlog!("Probing {} RPC node(s)...", self.urls.len());
        let mut tasks = Vec::new();
        for url in &self.urls {
            let client = self.client.clone();
            let url = url.clone();
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tasks.push(tokio::spawn(async move {
                let started = std::time::Instant::now();
                let result = Self::rpc_call_with_client(
                    client,
                    url.clone(),
                    id,
                    "eth_blockNumber",
                    json!([]),
                )
                .await;
                (url, started.elapsed(), result)
            }));
        }

        let mut fastest: Option<(String, Duration)> = None;
        for task in tasks {
            match task.await {
                Ok((url, elapsed, Ok(block))) => {
                    crate::rlog!(
                        "RPC OK {}ms {} block={}",
                        elapsed.as_millis(),
                        Self::short_url(&url),
                        block.as_str().unwrap_or("?")
                    );
                    if fastest.as_ref().map(|(_, t)| elapsed < *t).unwrap_or(true) {
                        fastest = Some((url, elapsed));
                    }
                }
                Ok((url, elapsed, Err(e))) => {
                    crate::rlog!(
                        "RPC FAIL {}ms {} {}",
                        elapsed.as_millis(),
                        Self::short_url(&url),
                        e
                    );
                }
                Err(e) => crate::rlog!("RPC probe task failed: {}", e),
            }
        }

        let (url, elapsed) = fastest.context("All RPC nodes failed eth_blockNumber")?;
        crate::rlog!(
            "Fastest RPC: {} ({}ms)",
            Self::short_url(&url),
            elapsed.as_millis()
        );
        Ok(url)
    }

    pub async fn sort_by_fastest_provider(&mut self) -> Result<()> {
        if self.urls.len() < 2 {
            return Ok(());
        }
        let mut results = Vec::new();
        for url in &self.urls {
            let client = self.client.clone();
            let url = url.clone();
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let client_clone = client.clone();
            let url_clone = url.clone();
            let elapsed = tokio::spawn(async move {
                let started = std::time::Instant::now();
                let result = Self::rpc_call_with_client(
                    client_clone,
                    url_clone,
                    id,
                    "eth_blockNumber",
                    json!([]),
                )
                .await;
                (started.elapsed(), result.is_ok())
            });
            results.push((url, elapsed));
        }

        let mut probed: Vec<(String, std::time::Duration)> = Vec::new();
        for (url, handle) in results {
            match handle.await {
                Ok((elapsed, true)) => {
                    crate::rlog!(
                        "RPC OK {}ms {}",
                        elapsed.as_millis(),
                        Self::short_url(&url)
                    );
                    probed.push((url, elapsed));
                }
                Ok((elapsed, false)) => {
                    crate::rlog!(
                        "RPC FAIL {}ms {} — excluded",
                        elapsed.as_millis(),
                        Self::short_url(&url)
                    );
                }
                Err(e) => crate::rlog!("RPC probe task failed: {}", e),
            }
        }

        probed.sort_by_key(|(_, t)| *t);
        let urls: Vec<String> = probed.into_iter().map(|(u, _)| u).collect();
        if urls.is_empty() {
            bail!("All RPC nodes failed");
        }
        crate::rlog!(
            "RPC order: {}",
            urls.iter()
                .map(|u| Self::short_url(u))
                .collect::<Vec<_>>()
                .join(" > ")
        );
        self.urls = urls;
        Ok(())
    }

    async fn rpc_call(
        &self,
        url: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let resp = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("RPC request failed via {}", Self::short_url(url)))?;
        let status = resp.status();
        let text = resp.text().await.with_context(|| {
            format!("failed to read RPC response from {}", Self::short_url(url))
        })?;
        if !status.is_success() {
            bail!(
                "RPC HTTP {} from {}: {}",
                status,
                Self::short_url(url),
                crate::truncate_str(&text, 240)
            );
        }
        let data: serde_json::Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "failed to parse RPC response from {}: {}",
                Self::short_url(url),
                crate::truncate_str(&text, 240)
            )
        })?;
        if let Some(error) = data.get("error") {
            bail!(
                "RPC {} via {} error: {}",
                method,
                Self::short_url(url),
                error
            );
        }
        data.get("result").cloned().with_context(|| {
            format!(
                "RPC {} via {}: no result in response",
                method,
                Self::short_url(url)
            )
        })
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let max_urls = self.urls.len().min(3);
        if max_urls == 0 {
            bail!("No RPC URLs configured (method {method})");
        }
        let mut errors: Vec<String> = Vec::new();
        for (i, url) in self.urls.iter().take(max_urls).enumerate() {
            match tokio::time::timeout(Duration::from_secs(5), self.rpc_call(url, method, params.clone())).await {
                Ok(Ok(result)) => return Ok(result),
                Ok(Err(e)) => {
                    let msg = format!("{} via {}: {}", method, Self::short_url(url), e);
                    crate::rlog!("RPC fail: {}", msg);
                    errors.push(msg);
                    if i + 1 >= max_urls {
                        bail!(
                            "All RPC {} attempts failed ({} node(s)): {}",
                            method,
                            max_urls,
                            errors.join(" | ")
                        );
                    }
                }
                Err(_) => {
                    let msg = format!("{} via {}: timeout 5s", method, Self::short_url(url));
                    crate::rlog!("RPC fail: {}", msg);
                    errors.push(msg);
                    if i + 1 >= max_urls {
                        bail!(
                            "All RPC {} attempts failed ({} node(s)): {}",
                            method,
                            max_urls,
                            errors.join(" | ")
                        );
                    }
                }
            }
        }
        bail!("No RPC URLs for method {method}")
    }

    pub async fn chain_id(&self) -> Result<u64> {
        let result = self.call("eth_chainId", json!([])).await?;
        let hex_str = result.as_str().context("chainId not a string")?;
        u64::from_str_radix(hex_str.strip_prefix("0x").unwrap_or(hex_str), 16)
            .context("invalid chainId")
    }

    pub async fn nonce(&self, address: &Address) -> Result<u64> {
        let result = self
            .call(
                "eth_getTransactionCount",
                json!([format!("{:?}", address), "pending"]),
            )
            .await?;
        let hex_str = result.as_str().context("nonce not a string")?;
        u64::from_str_radix(hex_str.strip_prefix("0x").unwrap_or(hex_str), 16)
            .context("invalid nonce")
    }

    pub async fn nonce_latest(&self, address: &Address) -> Result<u64> {
        let result = self
            .call(
                "eth_getTransactionCount",
                json!([format!("{:?}", address), "latest"]),
            )
            .await?;
        let hex_str = result.as_str().context("nonce not a string")?;
        u64::from_str_radix(hex_str.strip_prefix("0x").unwrap_or(hex_str), 16)
            .context("invalid nonce")
    }

    pub async fn block_timestamp(&self) -> Result<u64> {
        let result = self
            .call("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let hex_str = result
            .get("timestamp")
            .and_then(|v| v.as_str())
            .context("no timestamp")?;
        u64::from_str_radix(hex_str.strip_prefix("0x").unwrap_or(hex_str), 16)
            .context("invalid timestamp")
    }

    pub async fn balance(&self, address: &Address) -> Result<U256> {
        let result = self
            .call(
                "eth_getBalance",
                json!([format!("{:?}", address), "latest"]),
            )
            .await?;
        let hex_str = result.as_str().context("balance not a string")?;
        Ok(
            U256::from_str_radix(hex_str.strip_prefix("0x").unwrap_or(hex_str), 16)
                .context("invalid balance")?,
        )
    }

    pub async fn block_number(&self) -> Result<u64> {
        let result = self.call("eth_blockNumber", json!([])).await?;
        let hex_str = result.as_str().context("blockNumber not a string")?;
        u64::from_str_radix(hex_str.strip_prefix("0x").unwrap_or(hex_str), 16)
            .context("invalid blockNumber")
    }

    pub async fn estimate_gas(
        &self,
        from: &Address,
        to: &Address,
        value: U256,
        data: &Bytes,
    ) -> Result<u64> {
        let result = self
            .call(
                "eth_estimateGas",
                json!([{
                    "from": format!("{:?}", from),
                    "to": format!("{:?}", to),
                    "value": format!("0x{:x}", value),
                    "data": format!("0x{}", hex::encode(data)),
                }]),
            )
            .await?;
        let hex_str = result.as_str().context("gas not a string")?;
        u64::from_str_radix(hex_str.strip_prefix("0x").unwrap_or(hex_str), 16)
            .context("invalid gas")
    }

    pub async fn eth_call(&self, from: &Address, to: &Address, data: &Bytes) -> Result<Bytes> {
        // Omit `from` when zero — some RPC providers reject or mishandle from=0x0.
        let tx = if *from == Address::ZERO {
            json!({
                "to": format!("{:?}", to),
                "data": format!("0x{}", hex::encode(data)),
            })
        } else {
            json!({
                "from": format!("{:?}", from),
                "to": format!("{:?}", to),
                "data": format!("0x{}", hex::encode(data)),
            })
        };
        let result = self
            .call("eth_call", json!([tx, "latest"]))
            .await?;
        let hex_str = result.as_str().context("eth_call result not a string")?;
        Ok(Bytes::from(
            hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str)).context("invalid hex")?,
        ))
    }

    pub async fn get_code(&self, address: &Address) -> Result<Bytes> {
        let result = self
            .call("eth_getCode", json!([format!("{:?}", address), "latest"]))
            .await?;
        let hex_str = result.as_str().context("code not a string")?;
        Ok(Bytes::from(
            hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str)).context("invalid hex")?,
        ))
    }

    /// `eth_getStorageAt` — used for EIP-1967 proxy implementation resolution.
    pub async fn get_storage_at(&self, address: &Address, slot: B256) -> Result<B256> {
        let result = self
            .call(
                "eth_getStorageAt",
                json!([format!("{:?}", address), format!("{:?}", slot), "latest"]),
            )
            .await?;
        let hex_str = result.as_str().context("storage result not a string")?;
        let raw = hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))
            .context("invalid storage hex")?;
        if raw.len() != 32 {
            bail!("storage slot expected 32 bytes, got {}", raw.len());
        }
        Ok(B256::from_slice(&raw))
    }

    pub async fn fee_history(&self) -> Result<(U256, U256)> {
        let result = self
            .call("eth_feeHistory", json!(["0x1", "latest", [25.0]]))
            .await?;
        let base_fee = result
            .get("baseFeePerGas")
            .and_then(|v| v.as_array())
            .and_then(|a| a.last())
            .and_then(|v| v.as_str())
            .map(|s| {
                U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).unwrap_or_default()
            })
            .unwrap_or_default();
        let priority = result
            .get("reward")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .map(|s| {
                U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
                    .unwrap_or(U256::from(1_500_000_000u64))
            })
            .unwrap_or(U256::from(1_500_000_000u64));
        Ok((base_fee, priority))
    }

    pub async fn send_raw_transaction(&self, raw: &Bytes) -> Result<B256> {
        let urls: Vec<String> = self.urls.iter().take(3).cloned().collect();
        let max_attempts = urls.len();
        if max_attempts == 0 {
            bail!("No RPC URLs configured");
        }

        let mut tasks = Vec::new();
        for (attempt, url) in urls.into_iter().enumerate() {
            crate::rlog!(
                "RPC send attempt {}/{} via {}",
                attempt + 1,
                max_attempts,
                Self::short_url(&url)
            );
            let client = self.client.clone();
            let raw_hex = format!("0x{}", hex::encode(raw));
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tasks.push(tokio::spawn(async move {
                let result = Self::rpc_call_with_client(
                    client,
                    url.clone(),
                    id,
                    "eth_sendRawTransaction",
                    json!([raw_hex]),
                )
                .await;
                (url, result)
            }));
        }

        let mut tasks: Vec<Option<_>> = tasks.into_iter().map(Some).collect();
        let mut last_error = None;
        for i in 0..tasks.len() {
            let Some(handle) = tasks[i].take() else {
                continue;
            };
            match handle.await {
                Ok((url, Ok(result))) => {
                    let hex_str = result.as_str().context("tx hash not a string")?;
                    let hash = hex_str.parse().context("invalid tx hash")?;
                    crate::rlog!("RPC send OK via {}", Self::short_url(&url));
                    for task in &mut tasks {
                        if let Some(handle) = task.take() {
                            handle.abort();
                        }
                    }
                    return Ok(hash);
                }
                Ok((url, Err(e))) => {
                    let msg = format!(
                        "eth_sendRawTransaction via {}: {}",
                        Self::short_url(&url),
                        e
                    );
                    crate::rlog!("RPC send failed: {}", msg);
                    last_error = Some(msg);
                }
                Err(e) => {
                    let msg = format!("eth_sendRawTransaction task join: {e}");
                    crate::rlog!("{}", msg);
                    last_error = Some(msg);
                }
            }
        }
        bail!(
            "All RPC eth_sendRawTransaction attempts failed: {}",
            last_error.unwrap_or_else(|| "unknown".into())
        )
    }

    pub async fn transaction_receipt(&self, hash: &B256) -> Result<Option<serde_json::Value>> {
        let hash_hex = format!("0x{}", hex::encode(hash.as_slice()));
        let mut last_error = None;
        for (attempt, url) in self.urls.iter().take(3).enumerate() {
            match self
                .rpc_call(url, "eth_getTransactionReceipt", json!([hash_hex]))
                .await
            {
                Ok(result) => {
                    if attempt > 0 {
                        crate::rlog!("RPC receipt OK via {}", Self::short_url(url));
                    }
                    return if result.is_null() {
                        Ok(None)
                    } else {
                        Ok(Some(result))
                    };
                }
                Err(e) => {
                    crate::rlog!("RPC receipt failed via {}: {}", Self::short_url(url), e);
                    last_error = Some(e);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
        }
        bail!(
            "All RPC receipt attempts failed: {}",
            last_error.map(|e| e.to_string()).unwrap_or_default()
        )
    }

    pub async fn wait_for_receipt(
        &self,
        hash: &B256,
        timeout_secs: u64,
    ) -> Result<serde_json::Value> {
        self.wait_for_any_receipt(std::slice::from_ref(hash), timeout_secs)
            .await
            .map(|(_, receipt)| receipt)
    }

    /// Poll until **any** of `hashes` has a receipt (RBF: original or replacement).
    /// Returns `(mined_hash, receipt)`.
    ///
    /// Uses wall `started` for warn timing (not remaining-until-deadline).
    pub async fn wait_for_any_receipt(
        &self,
        hashes: &[B256],
        timeout_secs: u64,
    ) -> Result<(B256, serde_json::Value)> {
        if hashes.is_empty() {
            bail!("no tx hashes to wait for");
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        let started = std::time::Instant::now();
        let mut poll_interval = Duration::from_millis(100);
        let mut warned = false;
        while std::time::Instant::now() < deadline {
            for hash in hashes {
                // Transient full-RPC failures must not abort the wait: the tx may
                // still confirm. Log and keep polling until the deadline.
                match self.transaction_receipt(hash).await {
                    Ok(Some(receipt)) => return Ok((*hash, receipt)),
                    Ok(None) => {}
                    Err(e) => {
                        crate::rlog!("receipt poll error (will retry): {}", e);
                    }
                }
            }
            let waited = started.elapsed();
            if !warned && waited >= Duration::from_secs(10) {
                let short = hex::encode(hashes[0].as_slice());
                let short = &short[..short.len().min(8)];
                crate::rlog!(
                    "[WARN] tx {} (+{} alts) pending {}s",
                    short,
                    hashes.len().saturating_sub(1),
                    waited.as_secs()
                );
                warned = true;
            }
            tokio::time::sleep(poll_interval).await;
            // Exponential backoff up to 1s between polls.
            if poll_interval < Duration::from_secs(1) {
                let next_ms = (poll_interval.as_millis() as u64).max(1).saturating_mul(2);
                poll_interval = Duration::from_millis(next_ms.min(1_000));
            }
        }
        bail!(
            "Receipt timeout after {}s for {} candidate tx hash(es), first={:?}",
            timeout_secs,
            hashes.len(),
            hashes.first()
        )
    }

    pub async fn race_send(&self, raw: &Bytes) -> Result<B256> {
        self.send_raw_transaction(raw).await
    }
}

#[derive(Debug, PartialEq)]
pub struct ReceiptInfo {
    pub success: bool,
    pub gas_used: u64,
    pub block_number: u64,
}

pub fn parse_receipt(receipt: &serde_json::Value) -> ReceiptInfo {
    fn hex_u64(v: &serde_json::Value, key: &str) -> u64 {
        v.get(key)
            .and_then(|v| v.as_str())
            .map(|s| u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).unwrap_or(0))
            .unwrap_or(0)
    }
    ReceiptInfo {
        success: hex_u64(receipt, "status") == 1,
        gas_used: hex_u64(receipt, "gasUsed"),
        block_number: hex_u64(receipt, "blockNumber"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_successful_receipt() {
        let receipt = json!({
            "status": "0x1",
            "gasUsed": "0x5208",
            "blockNumber": "0x1234"
        });
        let info = parse_receipt(&receipt);
        assert_eq!(info, ReceiptInfo { success: true, gas_used: 0x5208, block_number: 0x1234 });
    }

    #[test]
    fn parse_failed_receipt() {
        let receipt = json!({
            "status": "0x0",
            "gasUsed": "0x100",
            "blockNumber": "0x5678"
        });
        let info = parse_receipt(&receipt);
        assert!(!info.success);
        assert_eq!(info.gas_used, 0x100);
        assert_eq!(info.block_number, 0x5678);
    }

    #[test]
    fn parse_receipt_missing_fields() {
        let receipt = json!({});
        let info = parse_receipt(&receipt);
        assert_eq!(info, ReceiptInfo { success: false, gas_used: 0, block_number: 0 });
    }

    #[test]
    fn parse_receipt_no_prefix() {
        let receipt = json!({
            "status": "1",
            "gasUsed": "5208",
            "blockNumber": "123"
        });
        let info = parse_receipt(&receipt);
        assert!(info.success);
        assert_eq!(info.gas_used, 0x5208);
    }
}
