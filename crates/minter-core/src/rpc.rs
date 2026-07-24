use alloy_primitives::{Address, B256, U256, bytes::Bytes};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::time::Duration;

/// Default max RPC endpoints to fan out across for a single logical call.
const RPC_MAX_NODES: usize = 3;
/// Default per-attempt request timeout for a single endpoint.
const RPC_CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Default hedge delay: how long to wait for the lead node before also firing
/// the next endpoint in parallel on a hedged read. Small enough to hide a slow
/// lead node near T0, large enough not to spam every node on a healthy link.
const RPC_HEDGE_DELAY: Duration = Duration::from_millis(350);

/// Tunable RPC fan-out / timeout knobs. Defaults match the constants above;
/// overridable via env / settings keys so fast L2s can tighten timeouts and
/// slow proxies can loosen them without a code change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcTuning {
    pub max_nodes: usize,
    pub call_timeout: Duration,
    pub hedge_delay: Duration,
}

impl Default for RpcTuning {
    fn default() -> Self {
        Self {
            max_nodes: RPC_MAX_NODES,
            call_timeout: RPC_CALL_TIMEOUT,
            hedge_delay: RPC_HEDGE_DELAY,
        }
    }
}

impl RpcTuning {
    /// Read overrides from a key lookup (env or settings). Missing / invalid
    /// keys keep the default; values are clamped to sane ranges so a bad config
    /// can't disable fan-out or set a 0 s timeout.
    ///
    /// Keys: `RPC_MAX_NODES` (1..=10), `RPC_CALL_TIMEOUT_MS` (500..=60000),
    /// `RPC_HEDGE_DELAY_MS` (50..=5000).
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(get: F) -> Self {
        let mut t = Self::default();
        if let Some(n) = get("RPC_MAX_NODES").and_then(|s| s.trim().parse::<usize>().ok()) {
            t.max_nodes = n.clamp(1, 10);
        }
        if let Some(ms) = get("RPC_CALL_TIMEOUT_MS").and_then(|s| s.trim().parse::<u64>().ok()) {
            t.call_timeout = Duration::from_millis(ms.clamp(500, 60_000));
        }
        if let Some(ms) = get("RPC_HEDGE_DELAY_MS").and_then(|s| s.trim().parse::<u64>().ok()) {
            t.hedge_delay = Duration::from_millis(ms.clamp(50, 5_000));
        }
        t
    }
}

pub struct RpcClient {
    client: reqwest::Client,
    urls: Vec<String>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    tuning: RpcTuning,
}

impl Clone for RpcClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            urls: self.urls.clone(),
            next_id: self.next_id.clone(),
            tuning: self.tuning,
        }
    }
}

impl RpcClient {
    /// Build a direct (non-proxied) RPC client.
    ///
    /// PRODUCT DECISION (audit M1, 2026-07-24): JSON-RPC traffic
    /// (`eth_sendRawTransaction`, nonce, balance, receipts) is intentionally sent
    /// **direct**. Proxies are applied only to OpenSea SIWE auth (per-wallet,
    /// where IP-based 429 rate-limits matter). RPC here is a single shared
    /// multi-URL race client per run, so per-wallet sticky proxying isn't possible
    /// without a refactor, and routing the race through one proxy would add
    /// hot-path latency. `new_with_proxy` is used only by the opt-in
    /// "Probe networks via proxy" diagnostic. See docs/ARCHITECTURE.md + SECURITY.md.
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
            tuning: RpcTuning::from_lookup(|k| std::env::var(k).ok()),
        })
    }

    fn short_url(url: &str) -> String {
        // Keep scheme + host only; drop path/query, which may embed the provider
        // API key (e.g. Alchemy `/v2/<key>`). Never log the key — not even a tail
        // fragment of it (audit L1). ':' '/' '?' are ASCII, so byte finds/slices
        // land on char boundaries and cannot panic.
        if let Some(scheme_end) = url.find("://") {
            let after = &url[scheme_end + 3..];
            let host_end = after
                .find('/')
                .or_else(|| after.find('?'))
                .unwrap_or(after.len());
            let host = &after[..host_end];
            if host_end < after.len() {
                return format!("{}://{}/…", &url[..scheme_end], host);
            }
            return format!("{}://{}", &url[..scheme_end], host);
        }
        // Non-URL string: char-based shorten (never byte-slice; non-ASCII safe).
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
                crate::safe_truncate(&text, 240)
            );
        }
        let data: serde_json::Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "RPC {method}: bad JSON from {short}: {}",
                crate::safe_truncate(&text, 240)
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
                    crate::rlog!("RPC OK {}ms {}", elapsed.as_millis(), Self::short_url(&url));
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
                crate::safe_truncate(&text, 240)
            );
        }
        let data: serde_json::Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "failed to parse RPC response from {}: {}",
                Self::short_url(url),
                crate::safe_truncate(&text, 240)
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
        let max_urls = self.urls.len().min(self.tuning.max_nodes);
        if max_urls == 0 {
            bail!("No RPC URLs configured (method {method})");
        }
        let mut errors: Vec<String> = Vec::new();
        for (i, url) in self.urls.iter().take(max_urls).enumerate() {
            match tokio::time::timeout(
                self.tuning.call_timeout,
                self.rpc_call(url, method, params.clone()),
            )
            .await
            {
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

    /// Latency-optimized read for idempotent methods (nonce, timestamp, …).
    ///
    /// Fires the lead endpoint immediately; if it hasn't answered within
    /// [`RPC_HEDGE_DELAY`], the next endpoint is fired **in parallel** (and so
    /// on up to [`RPC_MAX_NODES`]). The first successful response wins and the
    /// rest are aborted. A fast error on one node triggers the next
    /// immediately. With a single URL this is identical to a plain timed call.
    ///
    /// Safe only for read-only methods: the request may be sent to more than
    /// one node, so never use this for `eth_sendRawTransaction`.
    async fn call_hedged(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let urls: Vec<String> = self
            .urls
            .iter()
            .take(self.tuning.max_nodes)
            .cloned()
            .collect();
        if urls.is_empty() {
            bail!("No RPC URLs configured (method {method})");
        }

        // One spawnable, self-timed attempt against `urls[idx]`.
        let client = self.client.clone();
        let counter = self.next_id.clone();
        let method_s = method.to_string();
        let call_timeout = self.tuning.call_timeout;
        let hedge_delay = self.tuning.hedge_delay;
        let mk = |idx: usize| {
            let client = client.clone();
            let url = urls[idx].clone();
            let id = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let method = method_s.clone();
            let params = params.clone();
            let call_timeout = call_timeout;
            async move {
                let res = tokio::time::timeout(
                    call_timeout,
                    Self::rpc_call_with_client(client, url.clone(), id, &method, params),
                )
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("timeout {}ms", call_timeout.as_millis())));
                (url, res)
            }
        };

        let mut set = tokio::task::JoinSet::new();
        set.spawn(mk(0));
        let mut spawned = 1usize;
        let mut errors: Vec<String> = Vec::new();

        loop {
            if spawned < urls.len() {
                tokio::select! {
                    biased;
                    joined = set.join_next() => {
                        match joined {
                            Some(Ok((url, Ok(val)))) => {
                                let _ = url;
                                return Ok(val);
                            }
                            Some(Ok((url, Err(e)))) => errors.push(format!(
                                "{} via {}: {}", method, Self::short_url(&url), e
                            )),
                            Some(Err(e)) => errors.push(format!("{method} task join: {e}")),
                            None => {}
                        }
                        // Lead attempt already resolved (fast error) — fan out now.
                        if set.is_empty() && spawned < urls.len() {
                            set.spawn(mk(spawned));
                            spawned += 1;
                        }
                    }
                    _ = tokio::time::sleep(hedge_delay) => {
                        set.spawn(mk(spawned));
                        spawned += 1;
                    }
                }
            } else {
                match set.join_next().await {
                    Some(Ok((_url, Ok(val)))) => return Ok(val),
                    Some(Ok((url, Err(e)))) => {
                        errors.push(format!("{} via {}: {}", method, Self::short_url(&url), e))
                    }
                    Some(Err(e)) => errors.push(format!("{method} task join: {e}")),
                    None => break,
                }
            }
        }
        bail!(
            "All RPC {method} attempts failed ({} node(s)): {}",
            spawned,
            errors.join(" | ")
        )
    }

    pub async fn chain_id(&self) -> Result<u64> {
        let result = self.call_hedged("eth_chainId", json!([])).await?;
        let hex_str = result.as_str().context("chainId not a string")?;
        u64::from_str_radix(hex_str.strip_prefix("0x").unwrap_or(hex_str), 16)
            .context("invalid chainId")
    }

    pub async fn nonce(&self, address: &Address) -> Result<u64> {
        let result = self
            .call_hedged(
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
            .call_hedged(
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
            .call_hedged("eth_getBlockByNumber", json!(["latest", false]))
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
        let result = self.call("eth_call", json!([tx, "latest"])).await?;
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

    /// Broadcast a signed tx to up to `max_nodes` endpoints in parallel and
    /// return the first accepted hash. See [`send_raw_transaction_report`] for
    /// the per-node breakdown.
    pub async fn send_raw_transaction(&self, raw: &Bytes) -> Result<B256> {
        self.send_raw_transaction_report(raw).await.map(|r| r.hash)
    }

    /// Like [`send_raw_transaction`] but returns a [`SendReport`] describing
    /// which endpoint's response was used (`winner`), how many were tried, and
    /// the errors from the others (`losers`). Useful for latency/observability
    /// (which node is winning broadcasts) without changing the hash contract.
    pub async fn send_raw_transaction_report(&self, raw: &Bytes) -> Result<SendReport> {
        let urls: Vec<String> = self
            .urls
            .iter()
            .take(self.tuning.max_nodes)
            .cloned()
            .collect();
        let max_attempts = urls.len();
        if max_attempts == 0 {
            bail!("No RPC URLs configured");
        }

        // Broadcast to all endpoints in parallel; the first to COMPLETE
        // successfully wins. Each send is wrapped in a per-attempt timeout so a
        // hung node can't stall the winner up to the 30s client timeout, and the
        // JoinSet aborts the remaining tasks on drop (audit M4 — was a
        // sequential index-order await where a hung `urls[0]` blocked everything).
        let timeout = self.tuning.call_timeout;
        let mut set: tokio::task::JoinSet<(
            String,
            std::result::Result<serde_json::Value, String>,
        )> = tokio::task::JoinSet::new();
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
            set.spawn(async move {
                let res = match tokio::time::timeout(
                    timeout,
                    Self::rpc_call_with_client(
                        client,
                        url.clone(),
                        id,
                        "eth_sendRawTransaction",
                        json!([raw_hex]),
                    ),
                )
                .await
                {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err(format!("timeout {}s", timeout.as_secs().max(1))),
                };
                (url, res)
            });
        }

        let mut losers: Vec<String> = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((url, Ok(result))) => {
                    let hex_str = result.as_str().context("tx hash not a string")?;
                    let hash = hex_str.parse().context("invalid tx hash")?;
                    let winner = Self::short_url(&url);
                    if losers.is_empty() {
                        crate::rlog!("RPC send OK via {}", winner);
                    } else {
                        crate::rlog!(
                            "RPC send OK via {} (after {} failed: {})",
                            winner,
                            losers.len(),
                            losers.join("; ")
                        );
                    }
                    return Ok(SendReport {
                        hash,
                        winner,
                        nodes_tried: max_attempts,
                        losers,
                    });
                }
                Ok((url, Err(e))) => {
                    let msg = format!("{}: {}", Self::short_url(&url), e);
                    crate::rlog!("RPC send failed: {}", msg);
                    losers.push(msg);
                }
                Err(e) => {
                    let msg = format!("task join: {e}");
                    crate::rlog!("RPC send {}", msg);
                    losers.push(msg);
                }
            }
        }
        bail!(
            "All RPC eth_sendRawTransaction attempts failed ({} node(s)): {}",
            max_attempts,
            losers.join(" | ")
        )
    }

    pub async fn transaction_receipt(&self, hash: &B256) -> Result<Option<serde_json::Value>> {
        let hash_hex = format!("0x{}", hex::encode(hash.as_slice()));
        let mut last_error = None;
        for (attempt, url) in self.urls.iter().take(self.tuning.max_nodes).enumerate() {
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

/// Outcome of a parallel `eth_sendRawTransaction` broadcast: which endpoint's
/// response was used, how many were tried, and the errors from the others.
#[derive(Debug, Clone)]
pub struct SendReport {
    /// Accepted transaction hash.
    pub hash: B256,
    /// Short URL of the endpoint whose response we used.
    pub winner: String,
    /// Number of endpoints the broadcast fanned out to.
    pub nodes_tried: usize,
    /// `"shorturl: error"` for endpoints that errored before the winner.
    pub losers: Vec<String>,
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
        assert_eq!(
            info,
            ReceiptInfo {
                success: true,
                gas_used: 0x5208,
                block_number: 0x1234
            }
        );
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
        assert_eq!(
            info,
            ReceiptInfo {
                success: false,
                gas_used: 0,
                block_number: 0
            }
        );
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

    // —— Hedged reads ——
    // Minimal HTTP/1.1 JSON-RPC mock: replies with `body` after `delay`.
    fn spawn_mock(body: String, delay: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await; // best-effort read of request
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn ok_body(result_hex: &str) -> String {
        format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"{result_hex}\"}}")
    }

    #[tokio::test]
    async fn hedged_uses_fast_node_when_lead_is_slow() {
        // Lead node stalls 1.5s; second node answers instantly. Hedging should
        // fire the second after RPC_HEDGE_DELAY and return its value quickly.
        let slow = spawn_mock(ok_body("0x5"), Duration::from_millis(1500));
        let fast = spawn_mock(ok_body("0x9"), Duration::ZERO);
        let rpc = RpcClient::new(vec![slow, fast]);
        let t = std::time::Instant::now();
        let n = rpc.nonce(&Address::ZERO).await.unwrap();
        let elapsed = t.elapsed();
        assert_eq!(n, 9, "should take the fast node's value");
        assert!(
            elapsed < Duration::from_millis(1200),
            "must not block on the slow lead node (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn hedged_single_url_returns_value() {
        let only = spawn_mock(ok_body("0x11"), Duration::ZERO);
        let rpc = RpcClient::new(vec![only]);
        assert_eq!(rpc.nonce(&Address::ZERO).await.unwrap(), 0x11);
    }

    #[tokio::test]
    async fn hedged_fast_error_fails_over_immediately() {
        // Lead node returns a JSON-RPC error quickly → fail over to node 2
        // without waiting for the hedge delay.
        let bad = spawn_mock(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"boom\"}}"
                .to_string(),
            Duration::ZERO,
        );
        let good = spawn_mock(ok_body("0x7"), Duration::ZERO);
        let rpc = RpcClient::new(vec![bad, good]);
        assert_eq!(rpc.nonce(&Address::ZERO).await.unwrap(), 7);
    }

    #[test]
    fn tuning_defaults_and_overrides() {
        use std::collections::HashMap;
        // No keys → defaults match the constants.
        let def = RpcTuning::from_lookup(|_| None);
        assert_eq!(def, RpcTuning::default());
        assert_eq!(def.max_nodes, RPC_MAX_NODES);
        assert_eq!(def.call_timeout, RPC_CALL_TIMEOUT);
        assert_eq!(def.hedge_delay, RPC_HEDGE_DELAY);

        // Valid overrides are applied.
        let env: HashMap<&str, &str> = [
            ("RPC_MAX_NODES", "2"),
            ("RPC_CALL_TIMEOUT_MS", "1500"),
            ("RPC_HEDGE_DELAY_MS", "120"),
        ]
        .into_iter()
        .collect();
        let t = RpcTuning::from_lookup(|k| env.get(k).map(|s| s.to_string()));
        assert_eq!(t.max_nodes, 2);
        assert_eq!(t.call_timeout, Duration::from_millis(1500));
        assert_eq!(t.hedge_delay, Duration::from_millis(120));

        // Out-of-range values are clamped, not accepted verbatim.
        let bad: HashMap<&str, &str> = [
            ("RPC_MAX_NODES", "999"),
            ("RPC_CALL_TIMEOUT_MS", "1"),
            ("RPC_HEDGE_DELAY_MS", "99999"),
        ]
        .into_iter()
        .collect();
        let c = RpcTuning::from_lookup(|k| bad.get(k).map(|s| s.to_string()));
        assert_eq!(c.max_nodes, 10);
        assert_eq!(c.call_timeout, Duration::from_millis(500));
        assert_eq!(c.hedge_delay, Duration::from_millis(5_000));

        // Garbage → default retained.
        let g = RpcTuning::from_lookup(|_| Some("notanumber".to_string()));
        assert_eq!(g, RpcTuning::default());
    }

    #[tokio::test]
    async fn send_report_records_winner_and_losers() {
        // Lead node errors; second node accepts the tx. The report should name
        // the winner and carry the loser's error.
        let hash_hex = format!("0x{}", "11".repeat(32));
        let bad = spawn_mock(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"nope\"}}"
                .to_string(),
            Duration::ZERO,
        );
        let good = spawn_mock(ok_body(&hash_hex), Duration::ZERO);
        let good_short = RpcClient::short_url(&good);
        let rpc = RpcClient::new(vec![bad, good]);
        let raw = Bytes::from(vec![1u8, 2, 3]);
        let report = rpc.send_raw_transaction_report(&raw).await.unwrap();
        assert_eq!(report.hash, hash_hex.parse::<B256>().unwrap());
        assert_eq!(report.winner, good_short);
        assert_eq!(report.nodes_tried, 2);
        assert_eq!(report.losers.len(), 1, "the failed lead node is recorded");
        assert!(report.losers[0].contains("nope"));
    }
}
