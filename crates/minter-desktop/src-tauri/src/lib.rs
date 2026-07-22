use minter_core::api::{MintOptions, Session};
use minter_core::progress::{MintEvent, MintReporter};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// Forwards mint progress to the webview as `mint-event`.
struct TauriMintReporter {
    app: AppHandle,
    /// Once per mint run: first on-chain confirm only.
    first_confirm: Arc<AtomicBool>,
    /// Snapshot of Settings.beep at mint start (UI must honor).
    beep: bool,
}

/// Windows system sound on first confirm (WebView AudioContext is unreliable / silent).
#[cfg(windows)]
fn play_first_confirm_beep() {
    std::thread::spawn(|| {
        // MessageBeep is non-blocking system sound; Beep is a short audible chime.
        #[link(name = "user32")]
        extern "system" {
            fn MessageBeep(u_type: u32) -> i32;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn Beep(dw_freq: u32, dw_duration: u32) -> i32;
        }
        unsafe {
            let _ = MessageBeep(0x0000_0040); // MB_ICONASTERISK
            let _ = Beep(880, 160);
            let _ = Beep(1175, 200);
        }
    });
}

#[cfg(not(windows))]
fn play_first_confirm_beep() {
    print!("\x07");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

impl MintReporter for TauriMintReporter {
    fn report(&self, event: MintEvent) {
        // First on-chain confirm only (not every wallet, not dry-run OK).
        if let Some(ref st) = event.status {
            if minter_core::mint_ops::is_on_chain_confirm_status(st) {
                // false → true once; subsequent wallets skip emit
                if self
                    .first_confirm
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    if self.beep {
                        play_first_confirm_beep();
                    }
                    let payload = serde_json::json!({
                        "address": event.address,
                        "status": event.status,
                        "txHash": event.tx_hash,
                        "beep": self.beep,
                    });
                    let _ = self.app.emit("mint-first-confirm", &payload);
                }
            }
        }
        if let Some(ref d) = event.detail {
            let dl = d.to_lowercase();
            if dl.contains("re-auth") || dl.contains("401") {
                let _ = self.app.emit("mint-reauth", &event);
            }
        }
        if let Some(ref m) = event.message {
            let ml = m.to_lowercase();
            if ml.contains("401") || ml.contains("re-auth") {
                let _ = self.app.emit("mint-reauth", &event);
            }
        }
        let _ = self.app.emit("mint-event", &event);
    }
}

pub struct AppState {
    pub session: Mutex<Session>,
    /// Shared cancel flag for in-flight mint (Stop button).
    pub mint_cancel: Arc<AtomicBool>,
    pub mint_running: AtomicBool,
    /// Shared across reporter for one-shot first confirm per run.
    pub mint_first_confirm: Arc<AtomicBool>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            session: Mutex::new(Session::default_paths()),
            mint_cancel: Arc::new(AtomicBool::new(false)),
            mint_running: AtomicBool::new(false),
            mint_first_confirm: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Serialize)]
pub struct UiStatus {
    pub unlocked: bool,
    pub vault_label: String,
    pub wallet_count: usize,
    pub dry_run: bool,
    pub network: String,
    pub rpc: String,
    pub rpc_ok: bool,
    pub hint_title: String,
    pub hint_body: String,
}

#[tauri::command]
fn accept_burner(state: State<'_, Arc<AppState>>) {
    state.session.lock().accept_burner_warning();
}

#[tauri::command]
fn unlock(state: State<'_, Arc<AppState>>, password: String) -> Result<usize, String> {
    let mut s = state.session.lock();
    if !s.burner_accepted {
        return Err("Accept burner-wallet warning first".into());
    }
    s.unlock(&password).map_err(|e| e.to_string())
}

/// Clear signers + password from RAM (idle lock / manual lock).
#[tauri::command]
fn lock_vault(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    if state.mint_running.load(Ordering::SeqCst) {
        return Err("Cannot lock vault while a mint is running — Stop first".into());
    }
    state.session.lock().lock();
    Ok("Vault locked".into())
}

/// Pure policy: LIVE confirm required?
#[tauri::command]
fn live_confirm_required(state: State<'_, Arc<AppState>>, dry_run: bool) -> bool {
    let s = state.session.lock();
    minter_core::live_confirm_required(s.settings.require_live_confirm, dry_run)
}

/// Pure policy: warn multi-wallet without proxies?
#[tauri::command]
fn should_warn_no_proxy(wallet_count: u32, proxy_count: u32) -> bool {
    minter_core::should_warn_no_proxy(wallet_count as usize, proxy_count as usize)
}

#[tauri::command]
fn no_proxy_warn_message(wallet_count: u32) -> String {
    minter_core::no_proxy_multi_wallet_message(wallet_count as usize)
}

#[tauri::command]
fn get_status(state: State<'_, Arc<AppState>>) -> UiStatus {
    let s = state.session.lock();
    let (hint_title, hint_body) = if !s.has_wallets() {
        (
            "What you need next".into(),
            "Import or add burner wallets, then configure RPC in Settings.".into(),
        )
    } else if !s.rpc_configured() {
        (
            "What you need next".into(),
            "Open Settings → set Alchemy API key or custom RPC URLs, then Check Connection.".into(),
        )
    } else if s.dry_run {
        (
            "You're almost ready".into(),
            "Wallets + RPC OK. Dry Run is ON — open Mint Wizard to dry-run a drop.".into(),
        )
    } else {
        (
            "Live mode".into(),
            "LIVE is on — real transactions. Prefer Dry Run until you are sure.".into(),
        )
    };
    UiStatus {
        unlocked: s.is_unlocked(),
        vault_label: if s.is_unlocked() {
            "Unlocked".into()
        } else if s.vault_exists() {
            "Locked".into()
        } else {
            "No vault".into()
        },
        wallet_count: s.signers.len(),
        dry_run: s.dry_run,
        network: s.network_label.clone(),
        rpc: s.rpc_status.clone(),
        rpc_ok: s.rpc_configured(),
        hint_title,
        hint_body,
    }
}

#[tauri::command]
fn list_wallets(state: State<'_, Arc<AppState>>) -> Vec<minter_core::WalletInfo> {
    state.session.lock().list_wallets()
}

#[tauri::command]
fn add_key(state: State<'_, Arc<AppState>>, private_key: String) -> Result<String, String> {
    state
        .session
        .lock()
        .add_key(&private_key)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn import_file(state: State<'_, Arc<AppState>>, path: String) -> Result<usize, String> {
    state
        .session
        .lock()
        .import_file(std::path::Path::new(&path))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn import_files(state: State<'_, Arc<AppState>>, paths: Vec<String>) -> Result<usize, String> {
    let paths: Vec<std::path::PathBuf> = paths.into_iter().map(std::path::PathBuf::from).collect();
    state
        .session
        .lock()
        .import_files(&paths)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn import_keys_text(state: State<'_, Arc<AppState>>, text: String) -> Result<usize, String> {
    state
        .session
        .lock()
        .import_keys_text(&text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_proxies(state: State<'_, Arc<AppState>>) -> Vec<minter_core::ProxyListItem> {
    state.session.lock().list_proxies()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WalletBalancesInput {
    wallet_addresses: Option<Vec<String>>,
    /// Optional chain name (Raw Mint uses selected network).
    chain: Option<String>,
}

#[tauri::command]
async fn wallet_balances(
    state: State<'_, Arc<AppState>>,
    input: Option<WalletBalancesInput>,
) -> Result<Vec<minter_core::WalletBalanceRow>, String> {
    let session = state.session.lock().clone();
    let (addrs, chain) = match input {
        Some(i) => (i.wallet_addresses, i.chain),
        None => (None, None),
    };
    session
        .wallet_balances(addrs, chain.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeNetworksInput {
    /// Chain names e.g. ethereum, base, polygon. Empty → common set.
    chains: Option<Vec<String>>,
    /// Route JSON-RPC through first Settings proxy.
    via_proxy: Option<bool>,
}

#[tauri::command]
async fn probe_networks(
    state: State<'_, Arc<AppState>>,
    input: Option<ProbeNetworksInput>,
) -> Result<Vec<minter_core::NetworkProbeRow>, String> {
    let session = state.session.lock().clone();
    let (chains, via_proxy) = match input {
        Some(i) => (i.chains, i.via_proxy.unwrap_or(false)),
        None => (None, false),
    };
    session
        .probe_networks(chains, via_proxy)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProbeInput {
    chain: String,
    contract: String,
    quantity: Option<u32>,
    preset: Option<String>,
}

#[tauri::command]
async fn probe_raw(
    state: State<'_, Arc<AppState>>,
    input: RawProbeInput,
) -> Result<minter_core::RawProbeRow, String> {
    let session = state.session.lock().clone();
    session
        .probe_raw(
            &input.chain,
            &input.contract,
            input.quantity.unwrap_or(1),
            input.preset.as_deref().unwrap_or("mintBayPublic"),
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn probe_rpc(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<minter_core::RpcProbeResult>, String> {
    let mut session = {
        let s = state.session.lock();
        s.clone()
    };
    let res = session.probe_rpc().await.map_err(|e| e.to_string())?;
    *state.session.lock() = session;
    Ok(res)
}

/// Full settings DTO for desktop form (no need to hand-edit .env).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsDto {
    alchemy_api_key: String,
    /// Masked hint only (last 4); empty if no key stored.
    alchemy_masked: String,
    rpc_urls: String,
    rpc_url_ethereum: String,
    rpc_url_base: String,
    rpc_url_polygon: String,
    proxy_url: String,
    flashbots_relay_url: String,
    flashbots_max_blocks: u64,
    flashbots_resubmit_ms: u64,
    gas_limit: u64,
    use_gql: bool,
    priority_fee_gwei: String,
    base_fee_multiplier: String,
    gas_multiplier: String,
    max_retries: u32,
    quiet: bool,
    skip_preflight: bool,
    beep: bool,
    export_results: bool,
    dry_run: bool,
    require_live_confirm: bool,
    idle_lock_minutes: u32,
    fee_refresh_at_fire: String,
    config_path: String,
}

impl SettingsDto {
    fn from_session(s: &Session) -> Self {
        Self {
            alchemy_api_key: s.settings.alchemy_api_key.clone(),
            alchemy_masked: s.settings.alchemy_masked(),
            rpc_urls: s.settings.rpc_urls.clone(),
            rpc_url_ethereum: s.settings.rpc_url_ethereum.clone(),
            rpc_url_base: s.settings.rpc_url_base.clone(),
            rpc_url_polygon: s.settings.rpc_url_polygon.clone(),
            proxy_url: s.settings.proxy_url.clone(),
            flashbots_relay_url: s.settings.flashbots_relay_url.clone(),
            flashbots_max_blocks: s.settings.flashbots_max_blocks.max(1),
            flashbots_resubmit_ms: s.settings.flashbots_resubmit_ms.max(200),
            gas_limit: s.settings.gas_limit,
            use_gql: s.settings.use_gql,
            priority_fee_gwei: s.settings.priority_fee_gwei.clone(),
            base_fee_multiplier: s.settings.base_fee_multiplier.clone(),
            gas_multiplier: s.settings.gas_multiplier.clone(),
            max_retries: s.settings.max_retries,
            quiet: s.settings.quiet,
            skip_preflight: s.settings.skip_preflight,
            beep: s.settings.beep,
            export_results: s.settings.export_results,
            dry_run: s.dry_run,
            require_live_confirm: s.settings.require_live_confirm,
            idle_lock_minutes: s.settings.idle_lock_minutes,
            fee_refresh_at_fire: if s.settings.fee_refresh_at_fire.trim().is_empty() {
                "mainnetOnly".into()
            } else {
                s.settings.fee_refresh_at_fire.clone()
            },
            config_path: s.config_path().display().to_string(),
        }
    }
}

#[tauri::command]
fn get_settings(state: State<'_, Arc<AppState>>) -> SettingsDto {
    let s = state.session.lock();
    SettingsDto::from_session(&s)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveSettingsInput {
    /// Empty string keeps existing key; non-empty replaces.
    alchemy_api_key: Option<String>,
    rpc_urls: Option<String>,
    rpc_url_ethereum: Option<String>,
    rpc_url_base: Option<String>,
    rpc_url_polygon: Option<String>,
    proxy_url: Option<String>,
    gas_limit: Option<u64>,
    use_gql: Option<bool>,
    priority_fee_gwei: Option<String>,
    base_fee_multiplier: Option<String>,
    gas_multiplier: Option<String>,
    max_retries: Option<u32>,
    quiet: Option<bool>,
    skip_preflight: Option<bool>,
    beep: Option<bool>,
    export_results: Option<bool>,
    dry_run: Option<bool>,
    require_live_confirm: Option<bool>,
    idle_lock_minutes: Option<u32>,
    fee_refresh_at_fire: Option<String>,
    /// If true, clear alchemy key.
    clear_alchemy: Option<bool>,
    flashbots_relay_url: Option<String>,
    flashbots_max_blocks: Option<u64>,
    flashbots_resubmit_ms: Option<u64>,
}

#[tauri::command]
fn save_settings(
    state: State<'_, Arc<AppState>>,
    input: SaveSettingsInput,
) -> Result<String, String> {
    let mut s = state.session.lock();
    let mut settings = s.settings.clone();

    if input.clear_alchemy.unwrap_or(false) {
        settings.alchemy_api_key.clear();
    } else if let Some(key) = input.alchemy_api_key {
        let t = key.trim();
        // Keep existing when UI sends blank (user didn't re-type secret)
        if !t.is_empty() {
            settings.alchemy_api_key = t.to_string();
        }
    }
    if let Some(v) = input.rpc_urls {
        settings.rpc_urls = v;
    }
    if let Some(v) = input.rpc_url_ethereum {
        settings.rpc_url_ethereum = v;
    }
    if let Some(v) = input.rpc_url_base {
        settings.rpc_url_base = v;
    }
    if let Some(v) = input.rpc_url_polygon {
        settings.rpc_url_polygon = v;
    }
    if let Some(v) = input.proxy_url {
        settings.set_proxies_text(&v);
    }
    if let Some(v) = input.gas_limit {
        settings.gas_limit = v;
    }
    if let Some(v) = input.use_gql {
        settings.use_gql = v;
    }
    if let Some(v) = input.priority_fee_gwei {
        settings.priority_fee_gwei = v;
    }
    if let Some(v) = input.base_fee_multiplier {
        settings.base_fee_multiplier = v;
    }
    if let Some(v) = input.gas_multiplier {
        settings.gas_multiplier = v;
    }
    if let Some(v) = input.max_retries {
        settings.max_retries = v;
    }
    if let Some(v) = input.quiet {
        settings.quiet = v;
    }
    if let Some(v) = input.skip_preflight {
        settings.skip_preflight = v;
    }
    if let Some(v) = input.beep {
        settings.beep = v;
    }
    if let Some(v) = input.export_results {
        settings.export_results = v;
    }
    if let Some(v) = input.dry_run {
        settings.dry_run = v;
        s.dry_run = v;
    }
    if let Some(v) = input.require_live_confirm {
        settings.require_live_confirm = v;
    }
    if let Some(v) = input.idle_lock_minutes {
        settings.idle_lock_minutes = v.min(24 * 60);
    }
    if let Some(v) = input.fee_refresh_at_fire {
        let t = v.trim();
        settings.fee_refresh_at_fire = if t.is_empty() {
            "mainnetOnly".into()
        } else {
            minter_core::FeeRefreshMode::parse(t).as_str().to_string()
        };
    }
    if let Some(v) = input.flashbots_relay_url {
        settings.flashbots_relay_url = v.trim().to_string();
    }
    if let Some(v) = input.flashbots_max_blocks {
        if v > 0 {
            settings.flashbots_max_blocks = v.min(20);
        }
    }
    if let Some(v) = input.flashbots_resubmit_ms {
        if v >= 200 {
            settings.flashbots_resubmit_ms = v.min(10_000);
        }
    }

    s.apply_settings(settings);
    s.save_settings().map_err(|e| e.to_string())?;
    Ok(format!("Saved → {}", s.config_path().display()))
}

#[tauri::command]
fn apply_sniper(state: State<'_, Arc<AppState>>) {
    state.session.lock().apply_sniper_preset();
}

#[tauri::command]
fn security_status(state: State<'_, Arc<AppState>>) -> minter_core::SecurityStatus {
    state.session.lock().security_status()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SweepEthInput {
    chain: String,
    destination: String,
    dry_run: Option<bool>,
    confirm: Option<String>,
}

#[tauri::command]
async fn sweep_eth(
    state: State<'_, Arc<AppState>>,
    input: SweepEthInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    // Share the mint gate: concurrent broadcasts from the same vault reuse nonces.
    if state.mint_running.swap(true, Ordering::SeqCst) {
        return Err(minter_core::mint_busy_message().into());
    }
    let session = state.session.lock().clone();
    let result = session
        .sweep_eth(
            &input.chain,
            &input.destination,
            input.dry_run.unwrap_or(true),
            input.confirm.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| e.to_string());
    state.mint_running.store(false, Ordering::SeqCst);
    result
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SweepNftsInput {
    chain: String,
    contract: String,
    destination: String,
    dry_run: Option<bool>,
    confirm: Option<String>,
}

#[tauri::command]
async fn sweep_nfts(
    state: State<'_, Arc<AppState>>,
    input: SweepNftsInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    if state.mint_running.swap(true, Ordering::SeqCst) {
        return Err(minter_core::mint_busy_message().into());
    }
    let session = state.session.lock().clone();
    let result = session
        .sweep_nfts(
            &input.chain,
            &input.contract,
            &input.destination,
            input.dry_run.unwrap_or(true),
            input.confirm.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| e.to_string());
    state.mint_running.store(false, Ordering::SeqCst);
    result
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunMintInput {
    slug: String,
    quantity: Option<u32>,
    dry_run: Option<bool>,
    phase_index: Option<usize>,
    confirm: Option<String>,
    /// Selected vault addresses (if empty/absent → all wallets).
    wallet_addresses: Option<Vec<String>>,
    /// RPC chain override (ethereum, base, …).
    chain_override: Option<String>,
    /// Gas limit: omit = settings; 0 = auto estimate; n = fixed (manual).
    gas_limit: Option<u64>,
    /// Optional priority fee gwei override.
    priority_fee_gwei: Option<String>,
    /// address → proxy list index (manual mapping).
    proxy_overrides: Option<std::collections::HashMap<String, u32>>,
    /// Schedule: unix or ISO/RFC3339.
    at_time: Option<String>,
    /// Per-wallet quantity overrides.
    wallet_quantities: Option<std::collections::HashMap<String, u32>>,
    skip_estimate_on_open: Option<bool>,
    use_flashbots: Option<bool>,
}

#[tauri::command]
async fn run_mint(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    input: RunMintInput,
) -> Result<minter_core::MintRunSummary, String> {
    if state.mint_running.swap(true, Ordering::SeqCst) {
        return Err(minter_core::mint_busy_message().into());
    }
    let session = state.session.lock().clone();
    let dry_run = input.dry_run.unwrap_or(session.dry_run);
    let wallets = input.wallet_addresses.and_then(|v| {
        let v: Vec<String> = v.into_iter().filter(|s| !s.trim().is_empty()).collect();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    });
    let chain_override = input.chain_override.and_then(|c| {
        let t = c.trim().to_string();
        if t.is_empty() || t == "auto" {
            None
        } else {
            Some(t)
        }
    });
    // Validate at_time early for clearer UI errors
    let at_time = if let Some(ref raw) = input.at_time {
        match minter_core::parse_at_time_unix(raw) {
            Ok(None) => None,
            Ok(Some(ts)) => Some(ts.to_string()),
            Err(e) => {
                state.mint_running.store(false, Ordering::SeqCst);
                return Err(e);
            }
        }
    } else {
        None
    };
    let opts = MintOptions {
        slug: input.slug,
        quantity: input.quantity.unwrap_or(1).max(1),
        dry_run,
        auto_phase: input.phase_index.is_none(),
        phase_index: input.phase_index,
        at_time,
        use_gql: None,
        // LIVE path ignores preflight estimate (core always fixed-gas on live).
        skip_preflight: None,
        quiet: Some(false),
        priority_fee_gwei: input.priority_fee_gwei,
        gas_limit: input.gas_limit,
        wallet_addresses: wallets,
        chain_override,
        proxy_overrides: input.proxy_overrides,
        wallet_quantities: input.wallet_quantities,
        // Default true — no per-task "sniper" checkbox required.
        skip_estimate_on_open: Some(input.skip_estimate_on_open.unwrap_or(true)),
        use_flashbots: input.use_flashbots,
    };
    // Confirm string ignored by core (mint starts without typed CONFIRM).
    let confirm = input.confirm.unwrap_or_default();
    // Reset one-shot first-confirm gate for this run
    state.mint_first_confirm.store(false, Ordering::SeqCst);
    let beep = session.settings.beep;
    let reporter: Arc<dyn MintReporter> = Arc::new(TauriMintReporter {
        app,
        first_confirm: state.mint_first_confirm.clone(),
        beep,
    });
    let cancel = state.mint_cancel.clone();
    cancel.store(false, Ordering::SeqCst);
    let result = session
        .run_opensea_mint_cancellable(opts, &confirm, reporter, cancel)
        .await;
    state.mint_running.store(false, Ordering::SeqCst);
    result.map_err(|e| {
        let s = e.to_string();
        let lower = s.to_lowercase();
        if lower.contains("chain")
            && (lower.contains("mismatch") || lower.contains("wrong network"))
        {
            minter_core::chain_mismatch_message("collection", "RPC")
        } else if lower.contains("401") || lower.contains("unauthorized") {
            format!("{} ({s})", minter_core::reauth_required_message())
        } else {
            s
        }
    })
}

#[tauri::command]
fn cancel_mint(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    if !state.mint_running.load(Ordering::SeqCst) {
        return Ok("No mint running".into());
    }
    state.mint_cancel.store(true, Ordering::SeqCst);
    Ok("Stopping… cancel requested".into())
}

#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
fn results_dir() -> Result<String, String> {
    let p = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("results");
    let _ = std::fs::create_dir_all(&p);
    Ok(p.display().to_string())
}

#[tauri::command]
fn open_results_folder() -> Result<String, String> {
    let p = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("results");
    let _ = std::fs::create_dir_all(&p);
    open_folder(&p)?;
    Ok(p.display().to_string())
}

fn open_folder(p: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(p.as_os_str())
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("xdg-open").arg(p).spawn();
    }
    Ok(())
}

#[tauri::command]
fn open_logs_folder() -> Result<String, String> {
    let p = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("logs");
    let _ = std::fs::create_dir_all(&p);
    open_folder(&p)?;
    Ok(p.display().to_string())
}

#[tauri::command]
fn explorer_tx_url(chain: String, tx_hash: String) -> String {
    minter_core::explorer_tx_url(&chain, &tx_hash)
}

#[tauri::command]
fn parse_at_time(raw: String) -> Result<Option<i64>, String> {
    minter_core::parse_at_time_unix(&raw)
}

#[tauri::command]
fn mint_running(state: State<'_, Arc<AppState>>) -> bool {
    state.mint_running.load(Ordering::SeqCst)
}

#[tauri::command]
async fn test_auth(
    state: State<'_, Arc<AppState>>,
    all_wallets: bool,
) -> Result<Vec<minter_core::AuthTestRow>, String> {
    let session = state.session.lock().clone();
    session
        .test_auth(all_wallets)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WarmAuthInput {
    wallet_addresses: Option<Vec<String>>,
}

/// Pre-auth wallets into SIWE cache so mint Start hits CACHED OK.
#[tauri::command]
async fn warm_auth(
    state: State<'_, Arc<AppState>>,
    input: Option<WarmAuthInput>,
) -> Result<Vec<minter_core::AuthTestRow>, String> {
    let session = state.session.lock().clone();
    let addrs = input.and_then(|i| i.wallet_addresses);
    session.warm_auth(addrs).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn check_eligibility(
    state: State<'_, Arc<AppState>>,
    slug: String,
) -> Result<minter_core::EligibilityResult, String> {
    let session = state.session.lock().clone();
    session
        .check_eligibility(&slug)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckEligibilityWalletsInput {
    slug: String,
    /// If empty/absent → all unlocked wallets.
    wallet_addresses: Option<Vec<String>>,
}

/// Multi-wallet WL / eligibility (proxies by vault index).
#[tauri::command]
async fn check_eligibility_wallets(
    state: State<'_, Arc<AppState>>,
    input: CheckEligibilityWalletsInput,
) -> Result<minter_core::WalletEligibilityReport, String> {
    let session = state.session.lock().clone();
    let wallets = input.wallet_addresses.and_then(|v| {
        let v: Vec<String> = v.into_iter().filter(|s| !s.trim().is_empty()).collect();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    });
    session
        .check_eligibility_wallets(&input.slug, wallets)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn measure_latency(
    state: State<'_, Arc<AppState>>,
) -> Result<minter_core::LatencyReport, String> {
    let session = state.session.lock().clone();
    session.measure_latency().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn discover_raw_functions(
    state: State<'_, Arc<AppState>>,
    contract: String,
    chain: String,
) -> Result<Vec<minter_core::DiscoveredFunction>, String> {
    let session = state.session.lock().clone();
    session
        .discover_raw_functions(&contract, &chain)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMintInput {
    chain: String,
    contract: String,
    function: String,
    params: Option<Vec<String>>,
    value_eth: Option<String>,
    dry_run: Option<bool>,
    confirm: Option<String>,
    /// Selected vault addresses (if empty/absent → all wallets).
    wallet_addresses: Option<Vec<String>>,
    use_flashbots: Option<bool>,
    priority_fee_gwei: Option<String>,
    max_fee_gwei: Option<String>,
    gas_multiplier: Option<String>,
    gas_limit: Option<u64>,
}

#[tauri::command]
async fn raw_mint(
    state: State<'_, Arc<AppState>>,
    input: RawMintInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    if state.mint_running.swap(true, Ordering::SeqCst) {
        return Err(minter_core::mint_busy_message().into());
    }
    let session = state.session.lock().clone();
    let gas_mult = input
        .gas_multiplier
        .as_deref()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|m| *m > 0.0);
    let result = session
        .raw_mint(
            &input.chain,
            &input.contract,
            &input.function,
            input.params.unwrap_or_default(),
            input.value_eth.as_deref().unwrap_or("0"),
            input.dry_run.unwrap_or(true),
            input.confirm.as_deref().unwrap_or(""),
            input.wallet_addresses,
            input.use_flashbots.unwrap_or(false),
            input.priority_fee_gwei.as_deref(),
            input.max_fee_gwei.as_deref(),
            gas_mult,
            input.gas_limit,
        )
        .await
        .map_err(|e| e.to_string());
    state.mint_running.store(false, Ordering::SeqCst);
    result
}

#[tauri::command]
async fn raw_sniper(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    input: minter_core::RawSniperInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    if state.mint_running.swap(true, Ordering::SeqCst) {
        return Err(minter_core::mint_busy_message().into());
    }
    let session = state.session.lock().clone();
    state.mint_first_confirm.store(false, Ordering::SeqCst);
    let beep = session.settings.beep;
    let reporter: Arc<dyn MintReporter> = Arc::new(TauriMintReporter {
        app,
        first_confirm: state.mint_first_confirm.clone(),
        beep,
    });
    let cancel = state.mint_cancel.clone();
    cancel.store(false, Ordering::SeqCst);
    let result = session.raw_sniper(input, cancel, Some(reporter)).await;
    state.mint_running.store(false, Ordering::SeqCst);
    result.map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisperseInput {
    chain: String,
    from_address: String,
    to_addresses: Vec<String>,
    amount_eth: String,
    dry_run: Option<bool>,
}

#[tauri::command]
async fn disperse(
    state: State<'_, Arc<AppState>>,
    input: DisperseInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    if state.mint_running.swap(true, Ordering::SeqCst) {
        return Err(minter_core::mint_busy_message().into());
    }
    let session = state.session.lock().clone();
    let result = session
        .disperse(
            &input.chain,
            &input.from_address,
            input.to_addresses,
            &input.amount_eth,
            input.dry_run.unwrap_or(true),
        )
        .await
        .map_err(|e| e.to_string());
    state.mint_running.store(false, Ordering::SeqCst);
    result
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MulticallInput {
    chain: String,
    from_address: String,
    steps: Vec<minter_core::MulticallStepInput>,
    dry_run: Option<bool>,
    multicall_address: Option<String>,
}

#[tauri::command]
async fn multicall(
    state: State<'_, Arc<AppState>>,
    input: MulticallInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    if state.mint_running.swap(true, Ordering::SeqCst) {
        return Err(minter_core::mint_busy_message().into());
    }
    let session = state.session.lock().clone();
    let result = session
        .multicall(
            &input.chain,
            &input.from_address,
            input.steps,
            input.dry_run.unwrap_or(true),
            input.multicall_address,
        )
        .await
        .map_err(|e| e.to_string());
    state.mint_running.store(false, Ordering::SeqCst);
    result
}

#[tauri::command]
fn clear_auth_cache(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    state
        .session
        .lock()
        .clear_auth_cache()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_wallet(state: State<'_, Arc<AppState>>, address: String) -> Result<(), String> {
    state
        .session
        .lock()
        .remove_wallet(&address)
        .map_err(|e| e.to_string())
}

/// Toggle Dry Run / Live in session + persist to config.json.
#[tauri::command]
fn set_dry_run(state: State<'_, Arc<AppState>>, dry_run: bool) -> Result<bool, String> {
    let mut s = state.session.lock();
    s.dry_run = dry_run;
    s.settings.dry_run = dry_run;
    s.save_settings().map_err(|e| e.to_string())?;
    Ok(s.dry_run)
}

/// Native file picker (keys / proxy list).
#[tauri::command]
fn pick_file(
    title: Option<String>,
    filters: Option<Vec<String>>,
) -> Result<Option<String>, String> {
    let mut d = rfd::FileDialog::new().set_title(title.as_deref().unwrap_or("Open file"));
    let exts: Vec<String> = filters.unwrap_or_else(|| vec!["txt".into(), "*".into()]);
    let refs: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
    if !refs.is_empty() {
        d = d.add_filter("Text / list", &refs);
    }
    Ok(d.pick_file().map(|p| p.display().to_string()))
}

/// Multi-file picker (import several key lists at once).
#[tauri::command]
fn pick_files(title: Option<String>, filters: Option<Vec<String>>) -> Result<Vec<String>, String> {
    let mut d = rfd::FileDialog::new().set_title(title.as_deref().unwrap_or("Open files"));
    let exts: Vec<String> = filters.unwrap_or_else(|| vec!["txt".into(), "csv".into(), "*".into()]);
    let refs: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
    if !refs.is_empty() {
        d = d.add_filter("Text / list", &refs);
    }
    Ok(d.pick_files()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.display().to_string())
        .collect())
}

/// wallet_meta.json — groups + proxy map (no private keys).
fn wallet_meta_path(state: &AppState) -> std::path::PathBuf {
    let s = state.session.lock();
    s.config_path()
        .parent()
        .map(|p| p.join("wallet_meta.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("wallet_meta.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WalletMetaFile {
    version: u32,
    /// address → "A" | "B" | "C" | ""
    groups: std::collections::HashMap<String, String>,
    /// address → proxy list index
    proxy_map: std::collections::HashMap<String, u32>,
}

#[tauri::command]
fn load_wallet_meta(state: State<'_, Arc<AppState>>) -> Result<WalletMetaFile, String> {
    let path = wallet_meta_path(&state);
    if !path.exists() {
        return Ok(WalletMetaFile {
            version: 1,
            groups: Default::default(),
            proxy_map: Default::default(),
        });
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    match serde_json::from_str::<WalletMetaFile>(&raw) {
        Ok(mut f) => {
            if f.version == 0 {
                f.version = 1;
            }
            Ok(f)
        }
        Err(e) => {
            eprintln!("wallet_meta.json load error: {e}");
            Ok(WalletMetaFile {
                version: 1,
                groups: Default::default(),
                proxy_map: Default::default(),
            })
        }
    }
}

#[tauri::command]
fn save_wallet_meta(state: State<'_, Arc<AppState>>, file: WalletMetaFile) -> Result<(), String> {
    let path = wallet_meta_path(&state);
    let mut out = file;
    out.version = 1;
    let json = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| e.to_string())?;
    if std::fs::rename(&tmp, &path).is_err() {
        std::fs::write(&path, json.as_bytes()).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

/// Read a text file chosen by the user (proxies import helper).
#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

/// tasks.json next to config.json (no secrets — addresses + params only).
fn tasks_path(state: &AppState) -> std::path::PathBuf {
    let s = state.session.lock();
    s.config_path()
        .parent()
        .map(|p| p.join("tasks.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("tasks.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TasksFile {
    version: u32,
    tasks: Vec<serde_json::Value>,
}

#[tauri::command]
fn load_tasks(state: State<'_, Arc<AppState>>) -> Result<TasksFile, String> {
    let path = tasks_path(&state);
    if !path.exists() {
        return Ok(TasksFile {
            version: 1,
            tasks: vec![],
        });
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    match serde_json::from_str::<TasksFile>(&raw) {
        Ok(mut f) => {
            if f.version == 0 {
                f.version = 1;
            }
            Ok(f)
        }
        Err(e) => {
            // Corrupt file: return empty, do not wipe on disk.
            eprintln!("tasks.json load error: {e}");
            Ok(TasksFile {
                version: 1,
                tasks: vec![],
            })
        }
    }
}

#[tauri::command]
fn save_tasks(state: State<'_, Arc<AppState>>, file: TasksFile) -> Result<(), String> {
    let path = tasks_path(&state);
    let mut out = file;
    out.version = 1;
    let json = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic-ish: write temp then rename.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| e.to_string())?;
    if let Err(_) = std::fs::rename(&tmp, &path) {
        std::fs::write(&path, json.as_bytes()).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

/// runs_history.json — mint run summaries (no private keys).
fn runs_history_path(state: &AppState) -> std::path::PathBuf {
    let s = state.session.lock();
    s.config_path()
        .parent()
        .map(|p| p.join("runs_history.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("runs_history.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RunsHistoryFile {
    version: u32,
    /// Newest first.
    runs: Vec<serde_json::Value>,
}

#[tauri::command]
fn load_runs_history(state: State<'_, Arc<AppState>>) -> Result<RunsHistoryFile, String> {
    let path = runs_history_path(&state);
    if !path.exists() {
        return Ok(RunsHistoryFile {
            version: 1,
            runs: vec![],
        });
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    match serde_json::from_str::<RunsHistoryFile>(&raw) {
        Ok(mut f) => {
            if f.version == 0 {
                f.version = 1;
            }
            // Cap for safety
            if f.runs.len() > 200 {
                f.runs.truncate(200);
            }
            Ok(f)
        }
        Err(e) => {
            eprintln!("runs_history.json load error: {e}");
            Ok(RunsHistoryFile {
                version: 1,
                runs: vec![],
            })
        }
    }
}

#[tauri::command]
fn save_runs_history(state: State<'_, Arc<AppState>>, file: RunsHistoryFile) -> Result<(), String> {
    let path = runs_history_path(&state);
    let mut out = file;
    out.version = 1;
    if out.runs.len() > 200 {
        out.runs.truncate(200);
    }
    let json = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| e.to_string())?;
    if std::fs::rename(&tmp, &path).is_err() {
        std::fs::write(&path, json.as_bytes()).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

#[tauri::command]
async fn list_drop_phases(
    state: State<'_, Arc<AppState>>,
    slug: String,
) -> Result<minter_core::DropPhasesResult, String> {
    let session = state.session.lock().clone();
    session
        .list_drop_phases(&slug)
        .await
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Desktop: quiet core logs unless user set QUIET/DEBUG explicitly.
    if std::env::var("QUIET").is_err() && std::env::var("DEBUG").is_err() {
        // SAFETY: single-threaded before async runtime serves UI
        unsafe { std::env::set_var("QUIET", "1") };
    }
    let state = Arc::new(AppState::default());
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            accept_burner,
            unlock,
            lock_vault,
            live_confirm_required,
            should_warn_no_proxy,
            no_proxy_warn_message,
            get_status,
            list_wallets,
            add_key,
            import_file,
            import_files,
            import_keys_text,
            list_proxies,
            wallet_balances,
            probe_networks,
            load_wallet_meta,
            save_wallet_meta,
            pick_files,
            probe_rpc,
            get_settings,
            save_settings,
            apply_sniper,
            security_status,
            sweep_eth,
            sweep_nfts,
            run_mint,
            list_drop_phases,
            load_tasks,
            save_tasks,
            load_runs_history,
            save_runs_history,
            app_version,
            results_dir,
            open_results_folder,
            open_logs_folder,
            explorer_tx_url,
            parse_at_time,
            test_auth,
            warm_auth,
            check_eligibility,
            check_eligibility_wallets,
            measure_latency,
            discover_raw_functions,
            raw_mint,
            raw_sniper,
            probe_raw,
            disperse,
            multicall,
            clear_auth_cache,
            remove_wallet,
            set_dry_run,
            pick_file,
            read_text_file,
            cancel_mint,
            mint_running,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
