//! High-level API for CLI and desktop UIs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use alloy_primitives::Address;
use anyhow::{bail, Context, Result};
use zeroize::Zeroizing;

use crate::amount;
use crate::auth_cache::AuthCache;
use crate::opensea;
use crate::disperse::{self, DisperseConfig};
use crate::flashbots::FlashbotsConfig;
use crate::multicall::{self, MulticallConfig, MulticallStep, MULTICALL3};
use crate::progress::MintReporter;
use crate::raw_mint::{self, RawMintConfig};
use crate::raw_sniper::{
    self, CompareOp, DecodeKind, RawSniperConfig, SniperPreset, ValueMode, ViewRule,
};
use crate::rpc::RpcClient;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use crate::settings::Settings;
use crate::sweep::{self, SweepConfig, SweepEthConfig};
use crate::types::{GasParams, Signer, max_retries_from_env};
use crate::vault::Vault;
use crate::{BURNER_WARNING, NO_TELEMETRY};
use alloy_primitives::U256;

/// Shared session state for any UI.
#[derive(Debug, Clone)]
pub struct Session {
    vault_path: PathBuf,
    /// Primary settings store (`config.json`).
    config_path: PathBuf,
    /// Optional legacy `.env` (migration + CLI mint env map).
    env_path: PathBuf,
    password: Option<Zeroizing<String>>,
    pub signers: Vec<Signer>,
    pub settings: Settings,
    pub env: HashMap<String, String>,
    pub dry_run: bool,
    pub network_label: String,
    pub rpc_status: String,
    pub proxy_count: usize,
    pub burner_accepted: bool,
    pub last_drop: String,
    pub last_contract: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityStatus {
    pub vault_exists: bool,
    pub unlocked: bool,
    pub wallet_count: usize,
    pub dry_run: bool,
    pub burner_accepted: bool,
    pub burner_warning: String,
    pub no_telemetry: String,
    pub ready: bool,
    pub ready_reason: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RpcProbeResult {
    pub url_short: String,
    pub ok: bool,
    pub chain_id: Option<u64>,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletInfo {
    /// 1-based display index.
    pub index: usize,
    pub address: String,
    /// Short proxy label (auto by vault index, unless overridden by caller UI).
    pub proxy: String,
    /// 0-based proxy list index used for auto mapping.
    pub proxy_index: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletBalanceRow {
    pub address: String,
    pub balance_eth: String,
    pub balance_wei: String,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyListItem {
    pub index: u32,
    pub label: String,
}

impl Session {
    pub fn new(vault_path: impl Into<PathBuf>, env_path: impl Into<PathBuf>) -> Self {
        let env_path = env_path.into();
        let config_path = env_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("config.json"))
            .unwrap_or_else(|| PathBuf::from("config.json"));
        Self::with_paths(vault_path, config_path, env_path)
    }

    pub fn with_paths(
        vault_path: impl Into<PathBuf>,
        config_path: impl Into<PathBuf>,
        env_path: impl Into<PathBuf>,
    ) -> Self {
        let vault_path = vault_path.into();
        let config_path = config_path.into();
        let env_path = env_path.into();

        let settings = Settings::load(&config_path, Some(&env_path));
        let mut env = load_env_file(&env_path);
        // Settings (config.json) override / fill env map used by mint/RPC helpers
        for (k, v) in settings.to_env_map() {
            env.insert(k, v);
        }

        let mut rpc_status = "—".into();
        let mut network_label = "Not selected".into();
        let urls = collect_rpc_urls(&env);
        if !urls.is_empty() {
            rpc_status = format!("{}", urls.len());
            network_label = network_label_from_settings(&settings);
        }

        let dry_run = settings.dry_run;
        let proxy_count = settings.proxy_count();
        Self {
            vault_path,
            config_path,
            env_path,
            password: None,
            signers: Vec::new(),
            settings,
            env,
            dry_run,
            network_label,
            rpc_status,
            proxy_count,
            burner_accepted: false,
            last_drop: "—".into(),
            last_contract: "—".into(),
        }
    }

    pub fn default_paths() -> Self {
        Self::with_paths("keys.vault", "config.json", ".env")
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn env_path(&self) -> &Path {
        &self.env_path
    }

    pub fn vault_exists(&self) -> bool {
        self.vault_path.exists()
    }

    pub fn is_unlocked(&self) -> bool {
        self.password.is_some()
    }

    pub fn has_wallets(&self) -> bool {
        !self.signers.is_empty()
    }

    pub fn rpc_configured(&self) -> bool {
        let s = self.rpc_status.to_lowercase();
        !s.is_empty()
            && !s.contains("not set")
            && !s.contains("not configured")
            && !s.contains("not checked")
    }

    pub fn setup_ready(&self) -> bool {
        self.has_wallets() && self.rpc_configured()
    }

    pub fn not_ready_reason(&self) -> &'static str {
        if !self.has_wallets() {
            "no wallets"
        } else if !self.rpc_configured() {
            "no network/RPC"
        } else {
            "setup required"
        }
    }

    pub fn accept_burner_warning(&mut self) {
        self.burner_accepted = true;
    }

    pub fn unlock(&mut self, password: &str) -> Result<usize> {
        let vault = Vault::new(&self.vault_path);
        if !vault.exists() {
            // Create empty session password for new vault
            self.password = Some(Zeroizing::new(password.to_string()));
            self.signers.clear();
            return Ok(0);
        }
        let keys = vault.decrypt_keys(password)?;
        self.password = Some(Zeroizing::new(password.to_string()));
        self.rebuild_signers_from_keys(&keys);
        Ok(self.signers.len())
    }

    pub fn lock(&mut self) {
        self.password = None;
        self.signers.clear();
    }

    fn rebuild_signers_from_keys(&mut self, keys: &[Zeroizing<String>]) {
        self.signers = keys
            .iter()
            .filter_map(|k| k.strip_prefix("0x").unwrap_or(k).parse().ok())
            .collect();
    }

    pub fn list_wallets(&self) -> Vec<WalletInfo> {
        let proxies = self.proxy_manager();
        self.signers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let proxy = proxies.short(i);
                let proxy_index = if proxies.is_empty() {
                    None
                } else {
                    Some((i % proxies.len()) as u32)
                };
                WalletInfo {
                    index: i + 1,
                    address: format!("{:?}", s.address()),
                    proxy,
                    proxy_index,
                }
            })
            .collect()
    }

    pub fn list_proxies(&self) -> Vec<ProxyListItem> {
        self.proxy_manager()
            .list_short()
            .into_iter()
            .map(|(i, label)| ProxyListItem {
                index: i as u32,
                label,
            })
            .collect()
    }

    pub fn add_key(&mut self, private_key: &str) -> Result<String> {
        let pw = self
            .password
            .as_deref()
            .context("vault locked — unlock first")?;
        let vault = Vault::new(&self.vault_path);
        let addr = vault.add(private_key, pw)?;
        let keys = vault.decrypt_keys(pw)?;
        self.rebuild_signers_from_keys(&keys);
        Ok(format!("{:?}", addr))
    }

    pub fn import_file(&mut self, path: &Path) -> Result<usize> {
        let pw = self
            .password
            .as_deref()
            .context("vault locked — unlock first")?;
        let vault = Vault::new(&self.vault_path);
        let n = vault.import_from_file(path, pw)?;
        let keys = vault.decrypt_keys(pw)?;
        self.rebuild_signers_from_keys(&keys);
        Ok(n)
    }

    /// Import keys from free-form text (drag-drop / multi-file merge).
    pub fn import_keys_text(&mut self, text: &str) -> Result<usize> {
        let pw = self
            .password
            .as_deref()
            .context("vault locked — unlock first")?;
        let vault = Vault::new(&self.vault_path);
        let n = vault.import_from_text(text, pw)?;
        let keys = vault.decrypt_keys(pw)?;
        self.rebuild_signers_from_keys(&keys);
        Ok(n)
    }

    /// Import multiple key files; returns total new keys.
    pub fn import_files(&mut self, paths: &[PathBuf]) -> Result<usize> {
        let mut total = 0;
        for p in paths {
            total += self.import_file(p)?;
        }
        Ok(total)
    }

    /// Native balances for selected (or all) vault wallets.
    pub async fn wallet_balances(
        &self,
        wallet_addresses: Option<Vec<String>>,
    ) -> Result<Vec<WalletBalanceRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        let rpc = self.rpc_client()?;
        let filter: Option<std::collections::HashSet<String>> =
            wallet_addresses.and_then(|v| {
                let set: std::collections::HashSet<String> = v
                    .into_iter()
                    .map(|a| normalize_address(&a))
                    .filter(|a| a.len() > 2)
                    .collect();
                if set.is_empty() {
                    None
                } else {
                    Some(set)
                }
            });
        let mut rows = Vec::new();
        for s in &self.signers {
            let addr = s.address();
            let addr_s = format!("{:?}", addr);
            if let Some(ref f) = filter {
                if !f.contains(&normalize_address(&addr_s)) {
                    continue;
                }
            }
            match rpc.balance(&addr).await {
                Ok(wei) => {
                    let eth = amount::wei_to_eth_string(wei);
                    // "funded enough for gas+dust" — UI may raise threshold
                    let ok = wei > U256::from(10_000_000_000_000u64); // > 0.00001 ETH
                    rows.push(WalletBalanceRow {
                        address: addr_s,
                        balance_eth: eth,
                        balance_wei: wei.to_string(),
                        ok,
                        error: None,
                    });
                }
                Err(e) => {
                    rows.push(WalletBalanceRow {
                        address: addr_s,
                        balance_eth: "—".into(),
                        balance_wei: "0".into(),
                        ok: false,
                        error: Some(e.to_string()),
                    });
                }
            }
        }
        Ok(rows)
    }

    pub fn remove_wallet(&mut self, address: &str) -> Result<()> {
        let pw = self
            .password
            .as_deref()
            .context("vault locked — unlock first")?;
        let vault = Vault::new(&self.vault_path);
        vault.remove(address, pw)?;
        let keys = vault.decrypt_keys(pw)?;
        self.rebuild_signers_from_keys(&keys);
        Ok(())
    }

    pub fn reload_env(&mut self) {
        self.settings = Settings::load(&self.config_path, Some(&self.env_path));
        self.env = load_env_file(&self.env_path);
        for (k, v) in self.settings.to_env_map() {
            self.env.insert(k, v);
        }
        self.dry_run = self.settings.dry_run;
        self.proxy_count = self.settings.proxy_count();
        self.refresh_rpc_label();
    }

    fn refresh_rpc_label(&mut self) {
        let urls = collect_rpc_urls(&self.env);
        if urls.is_empty() {
            self.rpc_status = "—".into();
            self.network_label = "Not selected".into();
        } else {
            self.rpc_status = format!("{}", urls.len());
            self.network_label = network_label_from_settings(&self.settings);
        }
    }

    /// Persist settings to config.json and sync in-memory env map.
    pub fn save_settings(&mut self) -> Result<()> {
        self.settings.dry_run = self.dry_run;
        self.settings
            .save(&self.config_path)
            .context("write config.json")?;
        // Optional: keep .env mirror for CLI tools that still open .env
        let _ = self.settings.save_to_env_file(&self.env_path);
        for (k, v) in self.settings.to_env_map() {
            self.env.insert(k, v);
        }
        self.proxy_count = self.settings.proxy_count();
        self.refresh_rpc_label();
        Ok(())
    }

    /// Apply full settings snapshot from UI (keys, RPC, gas, flags).
    pub fn apply_settings(&mut self, settings: Settings) {
        self.settings = settings;
        self.dry_run = self.settings.dry_run;
        for (k, v) in self.settings.to_env_map() {
            self.env.insert(k, v);
        }
        self.proxy_count = self.settings.proxy_count();
        self.refresh_rpc_label();
    }

    pub fn apply_sniper_preset(&mut self) {
        self.settings.apply_sniper_preset();
    }

    pub fn security_status(&self) -> SecurityStatus {
        SecurityStatus {
            vault_exists: self.vault_exists(),
            unlocked: self.password.is_some(),
            wallet_count: self.signers.len(),
            dry_run: self.dry_run,
            burner_accepted: self.burner_accepted,
            burner_warning: BURNER_WARNING.to_string(),
            no_telemetry: NO_TELEMETRY.to_string(),
            ready: self.setup_ready(),
            ready_reason: if self.setup_ready() {
                "ready".into()
            } else {
                self.not_ready_reason().into()
            },
        }
    }

    /// Probe configured RPC URLs (up to 5).
    pub async fn probe_rpc(&mut self) -> Result<Vec<RpcProbeResult>> {
        // Always refresh env from settings before probe
        for (k, v) in self.settings.to_env_map() {
            self.env.insert(k, v);
        }
        let urls = collect_rpc_urls(&self.env);
        if urls.is_empty() {
            self.rpc_status = "—".into();
            self.network_label = "Not selected".into();
            bail!(
                "No RPC configured — open Settings and set Alchemy API key or custom RPC URLs"
            );
        }
        let mut out = Vec::new();
        for url in urls.iter().take(5) {
            let short = short_url(url);
            let client = RpcClient::new(vec![url.clone()]);
            let start = Instant::now();
            match client.chain_id().await {
                Ok(id) => {
                    let ms = start.elapsed().as_millis() as u64;
                    out.push(RpcProbeResult {
                        url_short: short,
                        ok: true,
                        chain_id: Some(id),
                        latency_ms: Some(ms),
                        error: None,
                    });
                }
                Err(e) => {
                    out.push(RpcProbeResult {
                        url_short: short,
                        ok: false,
                        chain_id: None,
                        latency_ms: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        }
        let ok_count = out.iter().filter(|r| r.ok).count();
        if ok_count > 0 {
            let best = out
                .iter()
                .filter(|r| r.ok)
                .min_by_key(|r| r.latency_ms.unwrap_or(u64::MAX));
            if let Some(b) = best {
                let cid = b.chain_id.unwrap_or(0);
                self.rpc_status = format!("OK {}ms", b.latency_ms.unwrap_or(0));
                self.network_label = chain_id_label(cid);
            }
        } else {
            self.rpc_status = "failed".into();
        }
        Ok(out)
    }

    pub fn max_retries(&self) -> u32 {
        if self.settings.max_retries > 0 {
            self.settings.max_retries
        } else {
            max_retries_from_env(&self.env)
        }
    }

    pub fn primary_address(&self) -> Option<Address> {
        self.signers.first().map(|s| s.address())
    }

    fn gas_params(&self) -> GasParams {
        GasParams::from_env(&self.env)
    }

    fn rpc_client(&self) -> Result<RpcClient> {
        let urls = collect_rpc_urls(&self.env);
        if urls.is_empty() {
            bail!("No RPC configured — set Alchemy or RPC in Settings");
        }
        Ok(RpcClient::new(urls))
    }

    /// RPC client pinned to a named chain (required for Raw Mint).
    fn rpc_client_for_chain(&self, chain: &str) -> Result<RpcClient> {
        let chain = chain.trim();
        if chain.is_empty() {
            bail!("Network required — select a chain for Raw Mint");
        }
        let urls = collect_rpc_urls_for_chain(&self.env, Some(chain), &[]);
        if urls.is_empty() {
            bail!(
                "No RPC for network '{chain}' — set Alchemy or custom RPC in Settings"
            );
        }
        Ok(RpcClient::new(urls))
    }

    /// Resolve expected chain id from name (if known).
    fn expected_chain_id(chain: &str) -> Option<u64> {
        let map = crate::types::chain_id_map();
        let key = chain.trim().to_lowercase();
        map.get(key.as_str()).copied().or_else(|| {
            // try without separators
            let compact = key.replace(['_', '-'], "");
            map.iter()
                .find(|(k, _)| k.replace(['_', '-'], "") == compact)
                .map(|(_, v)| *v)
        })
    }

    /// Sweep native ETH from all vault wallets to `destination`.
    /// `confirm` is ignored (kept for API stability; no typed CONFIRM gate).
    pub async fn sweep_eth(
        &self,
        destination: &str,
        dry_run: bool,
        _confirm: &str,
    ) -> Result<Vec<SweepResultRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        let dest: Address = destination
            .trim()
            .parse()
            .context("invalid destination address")?;
        let rpc = self.rpc_client()?;
        let config = SweepEthConfig {
            destination: dest,
            gas: self.gas_params(),
            dry_run,
        };
        let results = sweep::run_sweep_eth(&self.signers, &rpc, &config).await;
        Ok(results.into_iter().map(SweepResultRow::from).collect())
    }

    /// Sweep ERC-721 NFTs from all vault wallets to `destination`.
    /// `confirm` is ignored (kept for API stability; no typed CONFIRM gate).
    pub async fn sweep_nfts(
        &self,
        contract: &str,
        destination: &str,
        dry_run: bool,
        _confirm: &str,
    ) -> Result<Vec<SweepResultRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        let contract: Address = contract
            .trim()
            .parse()
            .context("invalid NFT contract address")?;
        let dest: Address = destination
            .trim()
            .parse()
            .context("invalid destination address")?;
        let rpc = self.rpc_client()?;
        let alchemy = {
            let k = self.settings.alchemy_api_key.trim();
            if k.is_empty() {
                None
            } else {
                Some(k.to_string())
            }
        };
        let config = SweepConfig {
            contract,
            destination: dest,
            gas: self.gas_params(),
            dry_run,
            alchemy_api_key: alchemy,
        };
        let results = sweep::run_sweep(&self.signers, &rpc, &config).await;
        Ok(results.into_iter().map(SweepResultRow::from).collect())
    }

    /// Build proxy manager from Settings (multi-line proxy list) or proxies.txt.
    pub fn proxy_manager(&self) -> crate::proxy::ProxyManager {
        let text = self.settings.proxy_url.trim();
        if text.is_empty() {
            crate::proxy::ProxyManager::load_default()
        } else {
            crate::proxy::ProxyManager::from_text(text)
        }
    }

    /// Run OpenSea mint (sniper path). No typed CONFIRM — Start runs live immediately.
    /// `confirm` is ignored (kept for API stability).
    pub async fn run_opensea_mint(
        &self,
        opts: MintOptions,
        _confirm: &str,
        reporter: std::sync::Arc<dyn crate::progress::MintReporter>,
    ) -> Result<crate::mint::MintRunSummary> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        if opts.slug.trim().is_empty() {
            bail!("Collection slug required");
        }
        let proxies = self.proxy_manager();
        let pw = self.password.as_ref().map(|z| z.as_str());
        crate::mint::run_opensea_mint(
            &self.signers,
            &self.env,
            &proxies,
            &opts,
            pw,
            reporter,
            None,
        )
        .await
    }

    /// OpenSea mint with optional cancel flag (desktop Stop button).
    /// No typed CONFIRM — Start runs live immediately. `confirm` is ignored.
    pub async fn run_opensea_mint_cancellable(
        &self,
        opts: MintOptions,
        _confirm: &str,
        reporter: std::sync::Arc<dyn crate::progress::MintReporter>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<crate::mint::MintRunSummary> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        if opts.slug.trim().is_empty() {
            bail!("Collection slug required");
        }
        cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        let proxies = self.proxy_manager();
        let pw = self.password.as_ref().map(|z| z.as_str());
        crate::mint::run_opensea_mint(
            &self.signers,
            &self.env,
            &proxies,
            &opts,
            pw,
            reporter,
            Some(cancel),
        )
        .await
    }

    /// Resolve chain id from RPC (preferred chain or ethereum / any).
    async fn resolve_chain_id(&self, preferred: Option<&str>) -> u64 {
        let mut urls = collect_rpc_urls_for_chain(&self.env, preferred, &[]);
        if urls.is_empty() && preferred.is_some() {
            urls = collect_rpc_urls_for_chain(&self.env, Some("ethereum"), &[]);
        }
        if urls.is_empty() {
            urls = collect_rpc_urls(&self.env);
        }
        if urls.is_empty() {
            return 1;
        }
        let rpc = RpcClient::new(urls);
        rpc.chain_id().await.unwrap_or(1)
    }

    /// Warm OpenSea SIWE auth into cache for selected (or all) wallets.
    /// Speeds up the next mint Start (CACHED OK path).
    pub async fn warm_auth(
        &self,
        wallet_addresses: Option<Vec<String>>,
    ) -> Result<Vec<AuthTestRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        let filter: Option<std::collections::HashSet<String>> =
            wallet_addresses.and_then(|v| {
                let set: std::collections::HashSet<String> = v
                    .into_iter()
                    .map(|a| normalize_address(&a))
                    .filter(|a| a.len() > 2)
                    .collect();
                if set.is_empty() {
                    None
                } else {
                    Some(set)
                }
            });
        let chain_id = self.resolve_chain_id(None).await;
        let proxies = self.proxy_manager();
        let pw = self.password.as_ref().map(|z| z.as_str());
        let mut cache = crate::auth_cache::AuthCache::load(pw);
        let mut rows = Vec::new();
        let selected: Vec<(usize, &Signer)> = self
            .signers
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                filter
                    .as_ref()
                    .map(|f| f.contains(&normalize_address(&format!("{:?}", s.address()))))
                    .unwrap_or(true)
            })
            .collect();
        if selected.is_empty() {
            bail!("No matching wallets for warm auth");
        }
        // Bounded concurrency similar to mint auth
        let conc = if proxies.is_empty() {
            2usize
        } else {
            proxies.len().clamp(2, 6)
        };
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(conc));
        let mut handles = Vec::new();
        for (vault_idx, signer) in selected {
            let addr = signer.address();
            let proxy = proxies.get(vault_idx).map(|s| s.to_string());
            let proxy_short = proxies.short(vault_idx);
            let signer = signer.clone();
            let sem = sem.clone();
            handles.push(tokio::spawn(async move {
                let _p = sem.acquire().await.ok();
                let start = Instant::now();
                let result =
                    opensea::siwe_auth(&addr, &signer, chain_id, proxy.as_deref()).await;
                (addr, proxy_short, start.elapsed().as_millis() as u64, result)
            }));
        }
        for h in handles {
            match h.await {
                Ok((addr, proxy_short, latency_ms, result)) => match result {
                    Ok(session) => {
                        let addr_s = format!("{:?}", addr);
                        cache.save(&addr_s, chain_id, &session.access_token);
                        let token = &session.access_token;
                        let masked = if token.len() > 16 {
                            format!("{}…{}", &token[..8], &token[token.len() - 8..])
                        } else {
                            "••••".into()
                        };
                        rows.push(AuthTestRow {
                            address: addr_s,
                            ok: true,
                            chain_id,
                            latency_ms,
                            token_masked: Some(masked),
                            error: None,
                            proxy: proxy_short,
                        });
                    }
                    Err(e) => {
                        rows.push(AuthTestRow {
                            address: format!("{:?}", addr),
                            ok: false,
                            chain_id,
                            latency_ms,
                            token_masked: None,
                            error: Some(e.to_string()),
                            proxy: proxy_short,
                        });
                    }
                },
                Err(e) => {
                    rows.push(AuthTestRow {
                        address: "join".into(),
                        ok: false,
                        chain_id,
                        latency_ms: 0,
                        token_masked: None,
                        error: Some(e.to_string()),
                        proxy: "—".into(),
                    });
                }
            }
        }
        Ok(rows)
    }

    /// Test OpenSea SIWE auth for each vault wallet (or primary only).
    pub async fn test_auth(&self, all_wallets: bool) -> Result<Vec<AuthTestRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        let chain_id = self.resolve_chain_id(Some("ethereum")).await;
        let proxies = self.proxy_manager();
        let signers: Vec<_> = if all_wallets {
            self.signers.iter().collect()
        } else {
            self.signers.iter().take(1).collect()
        };
        let mut rows = Vec::new();
        for (i, signer) in signers.into_iter().enumerate() {
            let addr = signer.address();
            let proxy = proxies.get(i).map(|s| s.to_string());
            let start = Instant::now();
            match opensea::siwe_auth(&addr, signer, chain_id, proxy.as_deref()).await {
                Ok(session) => {
                    let token = &session.access_token;
                    let masked = if token.len() > 16 {
                        format!("{}…{}", &token[..8], &token[token.len() - 8..])
                    } else if token.is_empty() {
                        "(empty)".into()
                    } else {
                        "••••".into()
                    };
                    rows.push(AuthTestRow {
                        address: format!("{:?}", addr),
                        ok: true,
                        chain_id,
                        latency_ms: start.elapsed().as_millis() as u64,
                        token_masked: Some(masked),
                        error: None,
                        proxy: proxies.short(i),
                    });
                }
                Err(e) => {
                    rows.push(AuthTestRow {
                        address: format!("{:?}", addr),
                        ok: false,
                        chain_id,
                        latency_ms: start.elapsed().as_millis() as u64,
                        token_masked: None,
                        error: Some(e.to_string()),
                        proxy: proxies.short(i),
                    });
                }
            }
        }
        Ok(rows)
    }

    /// OpenSea eligibility stages for primary wallet + slug.
    pub async fn check_eligibility(&self, slug: &str) -> Result<EligibilityResult> {
        let report = self
            .check_eligibility_wallets(slug, None)
            .await?;
        let row = report
            .wallets
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No wallets unlocked"))?;
        if let Some(err) = row.error {
            bail!("{}", err);
        }
        Ok(EligibilityResult {
            slug: report.slug,
            address: row.address,
            chain_id: report.chain_id,
            stages: row.stages,
        })
    }

    /// Multi-wallet OpenSea WL / eligibility check (uses proxies by vault index).
    ///
    /// `wallet_addresses`: if set and non-empty, only those vault wallets; else all unlocked.
    pub async fn check_eligibility_wallets(
        &self,
        slug: &str,
        wallet_addresses: Option<Vec<String>>,
    ) -> Result<WalletEligibilityReport> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        let slug = parse_collection_slug(slug);
        if slug.is_empty() {
            bail!("Collection slug required");
        }

        let filter: Option<std::collections::HashSet<String>> =
            wallet_addresses.and_then(|v| {
                let set: std::collections::HashSet<String> = v
                    .into_iter()
                    .map(|a| normalize_address(&a))
                    .filter(|a| a.len() > 2)
                    .collect();
                if set.is_empty() {
                    None
                } else {
                    Some(set)
                }
            });

        let selected: Vec<(usize, &Signer)> = self
            .signers
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                filter
                    .as_ref()
                    .map(|f| f.contains(&normalize_address(&format!("{:?}", s.address()))))
                    .unwrap_or(true)
            })
            .collect();

        if selected.is_empty() {
            bail!("No matching wallets in vault for the selection");
        }

        let chain_id = self.resolve_chain_id(None).await;
        let proxies = self.proxy_manager();
        let mut wallets = Vec::with_capacity(selected.len());

        for (vault_idx, signer) in selected {
            let addr = signer.address();
            let proxy = proxies.get(vault_idx).map(|s| s.to_string());
            let proxy_short = proxies.short(vault_idx);
            let start = Instant::now();
            match opensea::siwe_auth(&addr, signer, chain_id, proxy.as_deref()).await {
                Ok(auth) => {
                    match opensea::check_eligibility(&auth, &slug, &addr).await {
                        Ok(stages) => {
                            let stage_rows = stage_rows_from(&stages, None);
                            let is_elig = |e: &str| {
                                let e = e.to_lowercase();
                                e.starts_with("eligible") && !e.contains("not")
                            };
                            let eligible_labels: Vec<String> = stage_rows
                                .iter()
                                .filter(|s| is_elig(&s.eligible))
                                .map(|s| s.label.clone())
                                .collect();
                            let not_eligible_labels: Vec<String> = stage_rows
                                .iter()
                                .filter(|s| !is_elig(&s.eligible))
                                .map(|s| {
                                    format!("{} ({})", s.label, s.eligible)
                                })
                                .collect();
                            wallets.push(WalletEligibilityRow {
                                address: format!("{:?}", addr),
                                ok: true,
                                proxy: proxy_short,
                                latency_ms: start.elapsed().as_millis() as u64,
                                error: None,
                                stages: stage_rows,
                                eligible_labels,
                                not_eligible_labels,
                            });
                        }
                        Err(e) => {
                            wallets.push(WalletEligibilityRow {
                                address: format!("{:?}", addr),
                                ok: false,
                                proxy: proxy_short,
                                latency_ms: start.elapsed().as_millis() as u64,
                                error: Some(e.to_string()),
                                stages: vec![],
                                eligible_labels: vec![],
                                not_eligible_labels: vec![],
                            });
                        }
                    }
                }
                Err(e) => {
                    wallets.push(WalletEligibilityRow {
                        address: format!("{:?}", addr),
                        ok: false,
                        proxy: proxy_short,
                        latency_ms: start.elapsed().as_millis() as u64,
                        error: Some(format!("auth: {e}")),
                        stages: vec![],
                        eligible_labels: vec![],
                        not_eligible_labels: vec![],
                    });
                }
            }
        }

        Ok(WalletEligibilityReport {
            slug,
            chain_id,
            wallets,
        })
    }

    /// List drop stages for mint phase picker (collection_drop_info + recommended).
    pub async fn list_drop_phases(&self, slug: &str) -> Result<DropPhasesResult> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        let slug = parse_collection_slug(slug);
        if slug.is_empty() {
            bail!("Collection slug required");
        }
        let signer = &self.signers[0];
        let addr = signer.address();
        let chain_id = self.resolve_chain_id(None).await;
        let auth = opensea::siwe_auth(&addr, signer, chain_id, None)
            .await
            .context("OpenSea SIWE auth failed")?;
        let info = opensea::collection_drop_info(&auth, &slug, &addr)
            .await
            .context("collection drop info failed")?;
        if info.stages.is_empty() {
            bail!("No drop stages found for '{}'", slug);
        }
        let recommended = recommended_phase_index(&info);
        let stage_rows = stage_rows_from(&info.stages, Some(recommended));
        Ok(DropPhasesResult {
            slug: info.slug,
            name: info.name,
            chain: info.chain,
            address: format!("{:?}", addr),
            recommended_index: recommended,
            stages: stage_rows,
        })
    }

    /// Deep latency: RPC chainId + block + fees (+ nonce if wallets) and proxy health.
    pub async fn measure_latency(&self) -> Result<LatencyReport> {
        let urls = collect_rpc_urls(&self.env);
        if urls.is_empty() {
            bail!("No RPC configured — set Alchemy or RPC in Settings");
        }
        let mut rpc_rows = Vec::new();
        for url in urls.iter().take(8) {
            let short = short_url(url);
            let rpc = RpcClient::new(vec![url.clone()]);
            let mut row = LatencyRpcRow {
                url_short: short,
                ok: false,
                chain_id_ms: None,
                chain_id: None,
                block_ms: None,
                block_number: None,
                fees_ms: None,
                base_fee_gwei: None,
                priority_gwei: None,
                nonce_ms: None,
                nonce: None,
                error: None,
            };
            let start = Instant::now();
            match rpc.chain_id().await {
                Ok(id) => {
                    row.chain_id = Some(id);
                    row.chain_id_ms = Some(start.elapsed().as_millis() as u64);
                    row.ok = true;
                }
                Err(e) => {
                    row.error = Some(e.to_string());
                    rpc_rows.push(row);
                    continue;
                }
            }
            let start = Instant::now();
            if let Ok(b) = rpc.block_number().await {
                row.block_number = Some(b);
                row.block_ms = Some(start.elapsed().as_millis() as u64);
            }
            let start = Instant::now();
            if let Ok((base, prio)) = rpc.fee_history().await {
                row.fees_ms = Some(start.elapsed().as_millis() as u64);
                row.base_fee_gwei = Some((base / U256::from(1_000_000_000u64)).to::<u128>() as u64);
                row.priority_gwei =
                    Some((prio / U256::from(1_000_000_000u64)).to::<u128>() as u64);
            }
            if let Some(signer) = self.signers.first() {
                let addr = signer.address();
                let start = Instant::now();
                if let Ok(n) = rpc.nonce(&addr).await {
                    row.nonce = Some(n);
                    row.nonce_ms = Some(start.elapsed().as_millis() as u64);
                }
            }
            rpc_rows.push(row);
        }

        let proxies = self.proxy_manager();
        let n = self.signers.len().max(1).min(proxies.len().max(1));
        let proxy_health = proxies.probe_for_wallets(n).await;
        let proxy_rows: Vec<ProxyHealthRow> = proxy_health
            .into_iter()
            .map(|h| {
                let status = h.short_status();
                ProxyHealthRow {
                    label: h.label,
                    ok: h.ok,
                    latency_ms: h.latency_ms,
                    status,
                }
            })
            .collect();

        Ok(LatencyReport {
            rpc: rpc_rows,
            proxies: proxy_rows,
        })
    }

    /// Discover mint-like functions on a contract (for Raw Mint UI).
    pub async fn discover_raw_functions(
        &self,
        contract: &str,
        chain: &str,
    ) -> Result<Vec<DiscoveredFunction>> {
        if chain.trim().is_empty() {
            bail!("Network required — select a chain for Raw Mint");
        }
        let contract: Address = contract
            .trim()
            .parse()
            .context("invalid contract address")?;
        let rpc = self.rpc_client_for_chain(chain)?;
        if let Some(expected) = Self::expected_chain_id(chain) {
            let actual = rpc.chain_id().await.unwrap_or(0);
            if actual != 0 && actual != expected {
                bail!(
                    "RPC chainId {actual} does not match selected network {} (expected {expected})",
                    chain.trim()
                );
            }
        }
        let found = raw_mint::discover_functions(&rpc, &contract).await?;
        Ok(found
            .into_iter()
            .map(|(signature, source)| DiscoveredFunction { signature, source })
            .collect())
    }

    /// Raw contract mint for selected (or all) vault wallets.
    ///
    /// `wallet_addresses`: if set and non-empty, only those vault wallets; else all unlocked.
    pub fn flashbots_config(&self) -> FlashbotsConfig {
        let mut c = FlashbotsConfig::from_env(&self.env);
        if !self.settings.flashbots_relay_url.trim().is_empty() {
            c.relay_url = self.settings.flashbots_relay_url.trim().to_string();
        }
        if self.settings.flashbots_max_blocks > 0 {
            c.max_blocks = self.settings.flashbots_max_blocks.min(20);
        }
        if self.settings.flashbots_resubmit_ms >= 200 {
            c.resubmit_ms = self.settings.flashbots_resubmit_ms;
        }
        c
    }

    pub async fn raw_mint(
        &self,
        chain: &str,
        contract: &str,
        function: &str,
        params: Vec<String>,
        value_eth: &str,
        dry_run: bool,
        _confirm: &str,
        wallet_addresses: Option<Vec<String>>,
        use_flashbots: bool,
    ) -> Result<Vec<SweepResultRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        if chain.trim().is_empty() {
            bail!("Network required — select a chain for Raw Mint");
        }
        if use_flashbots {
            let c = chain.trim().to_lowercase();
            if c != "ethereum" && c != "mainnet" && c != "eth" {
                bail!("Flashbots bundle is only supported on Ethereum mainnet");
            }
        }
        let filter: Option<std::collections::HashSet<String>> =
            wallet_addresses.and_then(|v| {
                let set: std::collections::HashSet<String> = v
                    .into_iter()
                    .map(|a| normalize_address(&a))
                    .filter(|a| a.len() > 2)
                    .collect();
                if set.is_empty() {
                    None
                } else {
                    Some(set)
                }
            });
        let selected: Vec<Signer> = self
            .signers
            .iter()
            .filter(|s| {
                filter
                    .as_ref()
                    .map(|f| f.contains(&normalize_address(&format!("{:?}", s.address()))))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if selected.is_empty() {
            bail!("No matching wallets in vault for the selection");
        }
        let contract: Address = contract
            .trim()
            .parse()
            .context("invalid contract address")?;
        if function.trim().is_empty() {
            bail!("Function signature required");
        }
        let value = if value_eth.trim().is_empty() || value_eth.trim() == "0" {
            U256::ZERO
        } else {
            amount::eth_to_wei(value_eth.trim()).context("invalid ETH value")?
        };
        let rpc = self.rpc_client_for_chain(chain)?;
        if let Some(expected) = Self::expected_chain_id(chain) {
            let actual = rpc.chain_id().await.unwrap_or(0);
            if actual != 0 && actual != expected {
                bail!(
                    "RPC chainId {actual} does not match selected network {} (expected {expected})",
                    chain.trim()
                );
            }
        }
        let config = RawMintConfig {
            contract,
            function: function.trim().to_string(),
            params,
            value,
            gas: self.gas_params(),
            dry_run,
            use_flashbots,
            flashbots: self.flashbots_config(),
        };
        let results = raw_mint::run_raw_mint(&selected, &rpc, &config).await;
        Ok(results.into_iter().map(SweepResultRow::from).collect())
    }

    /// Raw sniper: wait for open (MintBay / rules / sim) then parallel mint.
    pub async fn raw_sniper(
        &self,
        input: RawSniperInput,
        cancel: Arc<AtomicBool>,
        reporter: Option<Arc<dyn MintReporter>>,
    ) -> Result<Vec<SweepResultRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        let chain = input.chain.trim();
        if chain.is_empty() {
            bail!("Network required — select a chain for Raw Sniper");
        }
        let filter: Option<std::collections::HashSet<String>> =
            input.wallet_addresses.and_then(|v| {
                let set: std::collections::HashSet<String> = v
                    .into_iter()
                    .map(|a| normalize_address(&a))
                    .filter(|a| a.len() > 2)
                    .collect();
                if set.is_empty() {
                    None
                } else {
                    Some(set)
                }
            });
        let selected: Vec<Signer> = self
            .signers
            .iter()
            .filter(|s| {
                filter
                    .as_ref()
                    .map(|f| f.contains(&normalize_address(&format!("{:?}", s.address()))))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if selected.is_empty() {
            bail!("No matching wallets in vault for the selection");
        }
        let contract: Address = input
            .contract
            .trim()
            .parse()
            .context("invalid contract address")?;

        let preset = match input.preset.as_deref().unwrap_or("mintBayPublic") {
            "simpleMintUint" | "simple" | "mint" => SniperPreset::SimpleMintUint,
            "custom" => SniperPreset::Custom,
            _ => SniperPreset::MintBayPublic,
        };
        let value_mode = match input.value_mode.as_deref().unwrap_or("auto") {
            "fixed" => ValueMode::Fixed,
            _ => ValueMode::Auto,
        };
        let fixed_value = {
            let s = input.value_eth.as_deref().unwrap_or("0").trim();
            if s.is_empty() || s == "0" {
                U256::ZERO
            } else {
                amount::eth_to_wei(s).context("invalid value ETH")?
            }
        };
        let qty = input.quantity.unwrap_or(1).max(1);
        let function = match preset {
            SniperPreset::MintBayPublic | SniperPreset::SimpleMintUint => {
                input
                    .function
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("mint(uint256)")
                    .to_string()
            }
            SniperPreset::Custom => {
                let f = input.function.as_deref().unwrap_or("").trim();
                if f.is_empty() {
                    bail!("Function signature required for Custom preset");
                }
                f.to_string()
            }
        };
        let at_time = raw_sniper::parse_sniper_at_time(input.at_time.as_deref())?;
        // Timeout: with at_time default 5 min; without default 120 min unless specified
        let timeout_secs = input.timeout_secs.unwrap_or(if at_time.is_some() {
            300
        } else {
            7200
        });
        let rules: Vec<ViewRule> = input
            .rules
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !r.function.trim().is_empty())
            .map(|r| ViewRule {
                function: r.function.trim().to_string(),
                params: r.params.unwrap_or_default(),
                decode: match r.decode.as_deref().unwrap_or("uint256") {
                    "bool" => DecodeKind::Bool,
                    _ => DecodeKind::Uint256,
                },
                op: match r.op.as_deref().unwrap_or("eq") {
                    "ne" | "!=" => CompareOp::Ne,
                    "gt" | ">" => CompareOp::Gt,
                    "gte" | ">=" => CompareOp::Gte,
                    "lt" | "<" => CompareOp::Lt,
                    "lte" | "<=" => CompareOp::Lte,
                    _ => CompareOp::Eq,
                },
                expected: r.expected.unwrap_or_else(|| "0".into()),
            })
            .collect();

        // Sim-open default: off MintBay, on Custom/Simple without rules
        let sim_open = input.sim_open.unwrap_or(match preset {
            SniperPreset::MintBayPublic => false,
            SniperPreset::SimpleMintUint => rules.is_empty(),
            SniperPreset::Custom => rules.is_empty(),
        });

        let rpc = self.rpc_client_for_chain(chain)?;
        if let Some(expected) = Self::expected_chain_id(chain) {
            let actual = rpc.chain_id().await.unwrap_or(0);
            if actual != 0 && actual != expected {
                bail!(
                    "RPC chainId {actual} does not match selected network {} (expected {expected})",
                    chain.trim()
                );
            }
        }

        let config = RawSniperConfig {
            contract,
            preset,
            function,
            params: input.params.unwrap_or_default(),
            quantity: qty as u64,
            value_mode,
            fixed_value,
            gas: self.gas_params(),
            dry_run: input.dry_run.unwrap_or(false),
            at_time,
            mint_before_at_time: input.mint_before_at_time.unwrap_or(true),
            timeout_secs,
            sim_open,
            rules,
            concurrency: input.concurrency.unwrap_or(16).max(1) as usize,
        };

        cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        let results =
            raw_sniper::run_raw_sniper(&selected, &rpc, &config, Some(cancel), reporter).await;
        Ok(results.into_iter().map(SweepResultRow::from).collect())
    }

    /// One-wallet Multicall3 batch (several calls, one tx).
    pub async fn multicall(
        &self,
        chain: &str,
        from_address: &str,
        steps: Vec<MulticallStepInput>,
        dry_run: bool,
        multicall_address: Option<String>,
    ) -> Result<Vec<SweepResultRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        if chain.trim().is_empty() {
            bail!("Network required — select a chain for Multicall");
        }
        if steps.is_empty() {
            bail!("Add at least one call");
        }
        let from_norm = normalize_address(from_address);
        let from = self
            .signers
            .iter()
            .find(|s| normalize_address(&format!("{:?}", s.address())) == from_norm)
            .cloned()
            .context("Source wallet not found in vault")?;

        let mut built: Vec<MulticallStep> = Vec::new();
        for (i, s) in steps.into_iter().enumerate() {
            let target: Address = s
                .target
                .trim()
                .parse()
                .with_context(|| format!("call #{}: invalid target", i + 1))?;
            let allow_failure = s.allow_failure.unwrap_or(false);
            let label = s
                .label
                .clone()
                .unwrap_or_else(|| format!("call{}", i + 1));
            let value_eth = s.value_eth.as_deref().unwrap_or("0");
            let step = if let Some(ref data) = s.calldata {
                if !data.trim().is_empty() {
                    multicall::step_from_calldata(
                        target,
                        data,
                        value_eth,
                        allow_failure,
                        label,
                    )?
                } else if let Some(ref fn_sig) = s.function {
                    multicall::step_from_function(
                        target,
                        fn_sig,
                        &s.params.unwrap_or_default(),
                        value_eth,
                        allow_failure,
                        label,
                    )?
                } else {
                    bail!("call #{}: need function or calldata", i + 1);
                }
            } else if let Some(ref fn_sig) = s.function {
                multicall::step_from_function(
                    target,
                    fn_sig,
                    &s.params.unwrap_or_default(),
                    value_eth,
                    allow_failure,
                    label,
                )?
            } else {
                bail!("call #{}: need function or calldata", i + 1);
            };
            built.push(step);
        }

        let mc_addr = if let Some(ref a) = multicall_address {
            let t = a.trim();
            if t.is_empty() {
                MULTICALL3
            } else {
                t.parse().context("invalid multicall address")?
            }
        } else {
            MULTICALL3
        };

        let rpc = self.rpc_client_for_chain(chain)?;
        if let Some(expected) = Self::expected_chain_id(chain) {
            let actual = rpc.chain_id().await.unwrap_or(0);
            if actual != 0 && actual != expected {
                bail!(
                    "RPC chainId {actual} does not match selected network {} (expected {expected})",
                    chain.trim()
                );
            }
        }

        let config = MulticallConfig {
            multicall: mc_addr,
            steps: built,
            gas: self.gas_params(),
            dry_run,
        };
        let result = multicall::run_multicall(&from, &rpc, &config).await;
        Ok(vec![SweepResultRow::from(result)])
    }

    /// Disperse native coin from one vault wallet to many destinations (fixed amount each).
    pub async fn disperse(
        &self,
        chain: &str,
        from_address: &str,
        to_addresses: Vec<String>,
        amount_eth: &str,
        dry_run: bool,
    ) -> Result<Vec<SweepResultRow>> {
        if self.signers.is_empty() {
            bail!("No wallets unlocked");
        }
        if chain.trim().is_empty() {
            bail!("Network required — select a chain for Disperse");
        }
        let from_norm = normalize_address(from_address);
        let from = self
            .signers
            .iter()
            .find(|s| normalize_address(&format!("{:?}", s.address())) == from_norm)
            .cloned()
            .context("Source wallet not found in vault")?;
        let destinations = disperse::parse_destinations(&to_addresses)?;
        // Remove source from destinations if present
        let from_addr = from.address();
        let destinations: Vec<_> = destinations
            .into_iter()
            .filter(|d| *d != from_addr)
            .collect();
        if destinations.is_empty() {
            bail!("Select at least one destination wallet (not the source)");
        }
        let amount = amount::eth_to_wei(amount_eth.trim()).context("invalid amount ETH")?;
        if amount.is_zero() {
            bail!("Amount must be greater than 0");
        }
        let rpc = self.rpc_client_for_chain(chain)?;
        if let Some(expected) = Self::expected_chain_id(chain) {
            let actual = rpc.chain_id().await.unwrap_or(0);
            if actual != 0 && actual != expected {
                bail!(
                    "RPC chainId {actual} does not match selected network {} (expected {expected})",
                    chain.trim()
                );
            }
        }
        let config = DisperseConfig {
            amount,
            gas: self.gas_params(),
            dry_run,
        };
        let results = disperse::run_disperse(&from, &destinations, &rpc, &config).await;
        Ok(results.into_iter().map(SweepResultRow::from).collect())
    }

    /// Clear encrypted OpenSea auth cache on disk.
    pub fn clear_auth_cache(&self) -> Result<String> {
        let pw = self.password.as_ref().map(|z| z.as_str());
        let mut cache = AuthCache::load(pw);
        let n = cache.len();
        cache.clear()?;
        Ok(format!("Cleared auth cache ({n} token(s))"))
    }
}

/// One call in a Multicall batch (from UI / Tauri).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MulticallStepInput {
    pub target: String,
    /// Function sig e.g. `mint(uint256)` (ignored if calldata set).
    pub function: Option<String>,
    pub params: Option<Vec<String>>,
    /// Raw hex calldata `0x…` (overrides function).
    pub calldata: Option<String>,
    pub value_eth: Option<String>,
    pub allow_failure: Option<bool>,
    pub label: Option<String>,
}

/// Input for raw contract sniper (desktop / API).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawSniperInput {
    pub chain: String,
    pub contract: String,
    pub preset: Option<String>,
    pub function: Option<String>,
    pub params: Option<Vec<String>>,
    pub quantity: Option<u32>,
    pub value_mode: Option<String>,
    pub value_eth: Option<String>,
    pub dry_run: Option<bool>,
    pub at_time: Option<String>,
    pub mint_before_at_time: Option<bool>,
    pub timeout_secs: Option<u64>,
    pub sim_open: Option<bool>,
    pub rules: Option<Vec<ViewRuleInput>>,
    pub wallet_addresses: Option<Vec<String>>,
    pub concurrency: Option<u32>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewRuleInput {
    pub function: String,
    pub params: Option<Vec<String>>,
    pub decode: Option<String>,
    pub op: Option<String>,
    pub expected: Option<String>,
}

/// Serializable sweep/mint row for desktop UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepResultRow {
    pub address: String,
    pub status: String,
    pub tx_hash: Option<String>,
    pub gas_used: Option<u64>,
    pub block_number: Option<u64>,
    pub error: Option<String>,
}

impl From<crate::types::MintResult> for SweepResultRow {
    fn from(r: crate::types::MintResult) -> Self {
        Self {
            address: format!("{:?}", r.address),
            status: r.status.to_string(),
            tx_hash: r.tx_hash.map(|h| format!("{:?}", h)),
            gas_used: r.gas_used,
            block_number: r.block_number,
            error: r.error,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthTestRow {
    pub address: String,
    pub ok: bool,
    pub chain_id: u64,
    pub latency_ms: u64,
    pub token_masked: Option<String>,
    pub error: Option<String>,
    pub proxy: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StageRow {
    /// 0-based index in the stages list (for MintOptions.phase_index).
    pub index: usize,
    pub label: String,
    pub stage_type: String,
    pub eligible: String,
    pub price_eth: Option<String>,
    pub max_mintable: Option<i64>,
    pub stage_index: Option<i64>,
    pub recommended: bool,
    /// Unix seconds when phase opens. None / 0 = already open or unknown.
    pub start_time: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EligibilityResult {
    pub slug: String,
    pub address: String,
    pub chain_id: u64,
    pub stages: Vec<StageRow>,
}

/// One wallet's OpenSea stage eligibility (WL check).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletEligibilityRow {
    pub address: String,
    pub ok: bool,
    pub proxy: String,
    pub latency_ms: u64,
    pub error: Option<String>,
    pub stages: Vec<StageRow>,
    /// Stage labels where wallet is eligible (WL / public / etc.).
    pub eligible_labels: Vec<String>,
    pub not_eligible_labels: Vec<String>,
}

/// Multi-wallet eligibility report for the WL Check page.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletEligibilityReport {
    pub slug: String,
    pub chain_id: u64,
    pub wallets: Vec<WalletEligibilityRow>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DropPhasesResult {
    pub slug: String,
    pub name: String,
    pub chain: String,
    pub address: String,
    pub recommended_index: usize,
    pub stages: Vec<StageRow>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyRpcRow {
    pub url_short: String,
    pub ok: bool,
    pub chain_id_ms: Option<u64>,
    pub chain_id: Option<u64>,
    pub block_ms: Option<u64>,
    pub block_number: Option<u64>,
    pub fees_ms: Option<u64>,
    pub base_fee_gwei: Option<u64>,
    pub priority_gwei: Option<u64>,
    pub nonce_ms: Option<u64>,
    pub nonce: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyHealthRow {
    pub label: String,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub status: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyReport {
    pub rpc: Vec<LatencyRpcRow>,
    pub proxies: Vec<ProxyHealthRow>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredFunction {
    pub signature: String,
    pub source: String,
}

fn recommended_phase_index(info: &opensea::CollectionInfo) -> usize {
    let stages = &info.stages;
    stages
        .iter()
        .enumerate()
        .filter(|(_, s)| opensea::stage_effective_eligible(s))
        .filter(|(_, s)| opensea::available_mint_quantity(info, s).unwrap_or(1) > 0)
        .min_by_key(|(_, s)| {
            let is_public = s.stage_type == "PUBLIC_SALE";
            let has_started = s.start_time.map(|t| t as i64).unwrap_or(0) <= 0;
            (is_public as usize, !has_started as usize, s.stage_index.unwrap_or(0))
        })
        .map(|(i, _)| i)
        .unwrap_or_else(|| {
            stages
                .iter()
                .position(opensea::stage_effective_eligible)
                .unwrap_or(0)
        })
}

fn stage_rows_from(stages: &[opensea::StageInfo], recommended: Option<usize>) -> Vec<StageRow> {
    stages
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let start_time = s.start_time.and_then(|t| {
                if t > 0.0 {
                    Some(t as i64)
                } else {
                    None
                }
            });
            StageRow {
                index: i,
                label: opensea::stage_label(s),
                stage_type: s.stage_type.clone(),
                eligible: opensea::stage_eligibility_label(s).to_string(),
                price_eth: s.price_eth.map(|p| format!("{p}")),
                max_mintable: s.max_mintable,
                stage_index: s.stage_index,
                recommended: recommended == Some(i),
                start_time,
            }
        })
        .collect()
}

/// Mint options independent of stdin / CLI prompts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintOptions {
    pub slug: String,
    pub quantity: u32,
    pub dry_run: bool,
    /// Prefer recommended phase when `phase_index` is None.
    pub auto_phase: bool,
    /// 0-based stage index (overrides auto when set).
    pub phase_index: Option<usize>,
    /// Schedule: RFC3339 or unix timestamp string.
    pub at_time: Option<String>,
    /// Override env USE_GQL when Some.
    pub use_gql: Option<bool>,
    /// Override env SKIP_PREFLIGHT when Some.
    pub skip_preflight: Option<bool>,
    /// Override env QUIET when Some.
    pub quiet: Option<bool>,
    /// Optional priority fee override (gwei string).
    pub priority_fee_gwei: Option<String>,
    /// Gas limit override. `None` = settings/env; `Some(0)` = auto estimate; `Some(n)` = fixed.
    pub gas_limit: Option<u64>,
    /// If set (non-empty), only these vault addresses mint (case-insensitive).
    pub wallet_addresses: Option<Vec<String>>,
    /// RPC chain override (e.g. "ethereum", "base"). None = use collection chain.
    pub chain_override: Option<String>,
    /// Optional wallet address → proxy list index (overrides vault-index mapping).
    pub proxy_overrides: Option<std::collections::HashMap<String, u32>>,
    /// Optional per-wallet mint quantity (address → qty). Default = `quantity`.
    pub wallet_quantities: Option<std::collections::HashMap<String, u32>>,
    /// If true, skip eth_estimateGas on open (use fixed gas limit) — faster, slightly riskier.
    pub skip_estimate_on_open: Option<bool>,
    /// When true, fire via Flashbots bundle (Ethereum mainnet only).
    pub use_flashbots: Option<bool>,
}

impl Default for MintOptions {
    fn default() -> Self {
        Self {
            slug: String::new(),
            quantity: 1,
            dry_run: true,
            auto_phase: true,
            phase_index: None,
            at_time: None,
            use_gql: None,
            skip_preflight: None,
            quiet: None,
            priority_fee_gwei: None,
            gas_limit: None,
            wallet_addresses: None,
            chain_override: None,
            proxy_overrides: None,
            wallet_quantities: None,
            skip_estimate_on_open: None,
            use_flashbots: None,
        }
    }
}

/// Human network label from configured RPCs (never "from Settings").
fn network_label_from_settings(settings: &Settings) -> String {
    if !settings.has_rpc() {
        return "Not selected".into();
    }
    let eth = !settings.rpc_url_ethereum.trim().is_empty();
    let base = !settings.rpc_url_base.trim().is_empty();
    let poly = !settings.rpc_url_polygon.trim().is_empty();
    let custom = !settings.rpc_urls.trim().is_empty();
    let alchemy = !settings.alchemy_api_key.trim().is_empty();
    let mut named: Vec<&str> = Vec::new();
    if eth {
        named.push("Ethereum");
    }
    if base {
        named.push("Base");
    }
    if poly {
        named.push("Polygon");
    }
    // Alchemy / multi / custom → operator picks chain per task
    if alchemy || custom || named.len() != 1 {
        return "Auto".into();
    }
    named[0].to_string()
}

fn chain_id_label(id: u64) -> String {
    match id {
        1 => "Ethereum".into(),
        8453 => "Base".into(),
        137 => "Polygon".into(),
        42161 => "Arbitrum".into(),
        10 => "Optimism".into(),
        56 => "BSC".into(),
        43114 => "Avalanche".into(),
        81457 => "Blast".into(),
        7777777 => "Zora".into(),
        33139 => "ApeChain".into(),
        360 => "Shape".into(),
        143 => "Monad".into(),
        4326 => "MegaETH".into(),
        4663 => "Robinhood Chain".into(),
        0 => "Not selected".into(),
        other => format!("chainId {other}"),
    }
}

/// Normalize address for comparison (`0x` + lowercase).
pub fn normalize_address(addr: &str) -> String {
    let a = addr.trim().to_lowercase();
    if a.starts_with("0x") {
        a
    } else {
        format!("0x{a}")
    }
}

/// Parse OpenSea collection URL or bare slug.
pub fn parse_collection_slug(raw: &str) -> String {
    let raw = raw.trim().trim_end_matches('/');
    let marker = "opensea.io/collection/";
    if let Some(idx) = raw.find(marker) {
        let after = &raw[idx + marker.len()..];
        after
            .split('/')
            .next()
            .unwrap_or(after)
            .split('?')
            .next()
            .unwrap_or(after)
            .to_string()
    } else {
        raw.to_string()
    }
}

fn add_unique_url(urls: &mut Vec<String>, url: String) {
    let url = url.trim().to_string();
    if !url.is_empty() && !urls.contains(&url) {
        urls.push(url);
    }
}

fn env_first<'a>(env: &'a HashMap<String, String>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| env.get(*name).map(|v| v.as_str()))
        .filter(|v| !v.trim().is_empty())
}

fn rpc_env_names(chain: Option<&str>) -> Vec<String> {
    let chain = match chain {
        Some(c) if !c.is_empty() => c,
        _ => return vec![],
    };
    let normalized = chain
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let mut aliases = vec![normalized.clone()];
    if normalized == "ETHEREUM" {
        aliases.push("MAINNET".to_string());
    }
    if normalized == "MATIC" {
        aliases.push("POLYGON".to_string());
    }
    let mut names = Vec::new();
    for alias in &aliases {
        names.push(format!("RPC_URL_{}", alias));
        names.push(format!("{}_RPC_URL", alias));
        names.push(format!("RPC_URLS_{}", alias));
        names.push(format!("{}_RPC_URLS", alias));
    }
    names
}

fn provider_chain_slugs(
    chain: &str,
) -> (
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
) {
    match chain.to_lowercase().as_str() {
        "ethereum" | "mainnet" => (Some("eth-mainnet"), Some("eth-mainnet"), Some("mainnet")),
        "base" => (
            Some("base-mainnet"),
            Some("open-platform:base"),
            Some("base"),
        ),
        "matic" | "polygon" => (
            Some("polygon-mainnet"),
            Some("polygon-mainnet"),
            Some("polygon"),
        ),
        "arbitrum" => (
            Some("arb-mainnet"),
            Some("arbitrum-mainnet"),
            Some("arbitrum"),
        ),
        "arbitrum_nova" | "arbitrum-nova" | "nova" => (None, None, Some("nova")),
        "optimism" => (Some("opt-mainnet"), Some("opt-mainnet"), Some("optimism")),
        "avalanche" => (
            Some("avax-mainnet"),
            Some("avalanche-mainnet"),
            Some("avalanche"),
        ),
        "bsc" => (None, Some("bsc-mainnet"), None),
        "blast" => (Some("blast-mainnet"), None, Some("blast")),
        "ape_chain" | "apechain" => (None, None, Some("apechain")),
        "monad" => (Some("monad-mainnet"), None, None),
        "megaeth" | "mega_eth" => (None, None, None),
        "robinhood" | "robinhood_chain" | "robinhood-chain" => {
            (Some("robinhood-mainnet"), None, None)
        }
        "shape" => (None, None, None),
        _ => (None, None, None),
    }
}

/// Well-known public RPC endpoints when no key/custom URL is configured.
fn public_rpc_fallback(chain: &str) -> Vec<&'static str> {
    match chain.to_lowercase().as_str() {
        "megaeth" | "mega_eth" => vec!["https://mainnet.megaeth.com/rpc"],
        "monad" => vec!["https://rpc.monad.xyz", "https://rpc1.monad.xyz"],
        "robinhood" | "robinhood_chain" | "robinhood-chain" => {
            vec!["https://rpc.mainnet.chain.robinhood.com"]
        }
        "shape" => vec!["https://mainnet.shape.network"],
        "zora" => vec!["https://rpc.zora.energy"],
        "apechain" | "ape_chain" => vec!["https://rpc.apechain.com/http"],
        "blast" => vec!["https://rpc.blast.io"],
        _ => vec![],
    }
}

fn provider_rpc_urls_for_chain(env: &HashMap<String, String>, chain: Option<&str>) -> Vec<String> {
    let Some(chain) = chain.filter(|c| !c.is_empty()) else {
        return vec![];
    };
    let (alchemy_slug, nodereal_slug, tenderly_slug) = provider_chain_slugs(chain);
    let mut urls = Vec::new();

    if let (Some(key), Some(slug)) = (
        env_first(env, &["ALCHEMY_API_KEY", "ALCHEMYAPIKEY", "alchemyapikey"]),
        alchemy_slug,
    ) {
        add_unique_url(
            &mut urls,
            format!("https://{}.g.alchemy.com/v2/{}", slug, key),
        );
    }

    if let (Some(key), Some(slug)) = (
        env_first(
            env,
            &["NODEREAL_API_KEY", "NODEREALAPIKEY", "noderealapikey"],
        ),
        nodereal_slug,
    ) {
        let url = if let Some(path) = slug.strip_prefix("open-platform:") {
            format!("https://open-platform.nodereal.io/{}/{}", key, path)
        } else {
            format!("https://{}.nodereal.io/v1/{}", slug, key)
        };
        add_unique_url(&mut urls, url);
    }

    if let (Some(key), Some(slug)) = (
        env_first(
            env,
            &[
                "TENDERLY_API_KEY",
                "TENDERLY_NODE_ACCESS_KEY",
                "TENDERLYAPIKEY",
                "tenderlyapikey",
            ],
        ),
        tenderly_slug,
    ) {
        add_unique_url(
            &mut urls,
            format!("https://{}.gateway.tenderly.co/{}", slug, key),
        );
    }

    urls
}

/// Collect RPC URLs for a collection chain (Settings / env / provider keys).
pub fn collect_rpc_urls_for_chain(
    env: &HashMap<String, String>,
    chain: Option<&str>,
    extra: &[String],
) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for url in extra {
        add_unique_url(&mut urls, url.clone());
    }
    for name in rpc_env_names(chain) {
        if let Some(value) = env.get(&name) {
            if name.contains("URLS") {
                for url in value.split(',') {
                    add_unique_url(&mut urls, url.to_string());
                }
            } else {
                add_unique_url(&mut urls, value.clone());
            }
        }
    }
    for url in provider_rpc_urls_for_chain(env, chain) {
        add_unique_url(&mut urls, url);
    }
    if let Some(c) = chain.filter(|c| !c.is_empty()) {
        for url in public_rpc_fallback(c) {
            add_unique_url(&mut urls, url.to_string());
        }
    }
    if let Some(url) = env.get("RPC_URL") {
        add_unique_url(&mut urls, url.clone());
    }
    if let Some(urls_str) = env.get("RPC_URLS") {
        for url in urls_str.split(',') {
            add_unique_url(&mut urls, url.to_string());
        }
    }
    // Merge generic collect only when still empty (no chain-specific + no public fallback)
    if urls.is_empty() {
        for u in collect_rpc_urls(env) {
            add_unique_url(&mut urls, u);
        }
    }
    urls
}

pub fn load_env_file(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if !path.exists() {
        return map;
    }
    if let Ok(iter) = dotenvy::from_path_iter(path) {
        for item in iter.flatten() {
            map.insert(item.0, item.1);
        }
    }
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            if let Some((k, v)) = parse_env_line(line) {
                map.insert(k, v);
            }
        }
    }
    map
}

fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line
        .strip_prefix("export ")
        .or_else(|| line.strip_prefix("export\t"))
        .unwrap_or(line)
        .trim();
    let (key, val) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let val = val.trim();
    let val = if val.len() >= 2
        && ((val.starts_with('"') && val.ends_with('"'))
            || (val.starts_with('\'') && val.ends_with('\'')))
    {
        val[1..val.len() - 1].to_string()
    } else {
        val.to_string()
    };
    Some((key.to_string(), val))
}

/// Collect common RPC URL env keys (simplified for desktop/core).
pub fn collect_rpc_urls(env: &HashMap<String, String>) -> Vec<String> {
    let mut urls = Vec::new();
    let push = |urls: &mut Vec<String>, u: &str| {
        let u = u.trim().to_string();
        if !u.is_empty() && !urls.contains(&u) {
            urls.push(u);
        }
    };
    for key in [
        "RPC_URL",
        "RPC_URLS",
        "RPC_URL_ETHEREUM",
        "RPC_URL_BASE",
        "RPC_URL_POLYGON",
        "ETHEREUM_RPC_URL",
        "BASE_RPC_URL",
        "POLYGON_RPC_URL",
        "RPC_URLS_ETHEREUM",
        "RPC_URLS_BASE",
    ] {
        if let Some(v) = env.get(key) {
            if key.contains("URLS") {
                for part in v.split(',') {
                    push(&mut urls, part);
                }
            } else {
                push(&mut urls, v);
            }
        }
    }
    // Provider auto-URLs
    if let Some(key) = env
        .get("ALCHEMY_API_KEY")
        .or_else(|| env.get("alchemyapikey"))
    {
        let key = key.trim();
        if !key.is_empty() && key != "YOUR_ALCHEMY_KEY" {
            push(
                &mut urls,
                &format!("https://eth-mainnet.g.alchemy.com/v2/{}", key),
            );
            push(
                &mut urls,
                &format!("https://base-mainnet.g.alchemy.com/v2/{}", key),
            );
            push(
                &mut urls,
                &format!("https://polygon-mainnet.g.alchemy.com/v2/{}", key),
            );
        }
    }
    urls
}

fn short_url(url: &str) -> String {
    if url.len() > 42 {
        format!("{}...{}", &url[..30], &url[url.len() - 8..])
    } else {
        url.to_string()
    }
}
