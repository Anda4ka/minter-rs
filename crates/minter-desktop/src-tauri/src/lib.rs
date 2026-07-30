use minter_core::api::{MintOptions, Session};
use minter_core::progress::{MintEvent, MintReporter};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// Native OS confirmation for irreversible / fund-moving actions.
///
/// The typed-`LIVE` ceremony lives in the webview, so it only protects against
/// operator misclicks: a compromised renderer can send `confirm: "LIVE"` itself.
/// This dialog is rendered by the OS from Rust, so the frontend can neither
/// forge nor suppress the answer — it is the trust boundary for spending funds.
///
/// Runs on the blocking pool: `show()` pumps a modal message loop and must not
/// block the Tauri event loop (that freezes the window behind the dialog).
async fn native_confirm(title: &str, body: String) -> Result<(), String> {
    let title = title.to_string();
    let answer = tauri::async_runtime::spawn_blocking(move || {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title(&title)
            .set_description(&body)
            .set_buttons(rfd::MessageButtons::YesNo)
            .show()
    })
    .await
    .map_err(|e| format!("confirm dialog failed: {e}"))?;
    // Any answer other than an explicit Yes is a refusal (window closed,
    // Esc, No) — fail closed.
    if matches!(answer, rfd::MessageDialogResult::Yes) {
        Ok(())
    } else {
        Err("Cancelled — not confirmed in the system dialog".into())
    }
}

/// Gate a live (non-dry-run) fund movement behind a native OS dialog.
///
/// `dry_run == true` runs are free to proceed: nothing is broadcast.
async fn confirm_live_spend(
    dry_run: bool,
    action: &str,
    details: &[(&str, String)],
) -> Result<(), String> {
    if dry_run {
        return Ok(());
    }
    let mut body =
        String::from("LIVE — this will broadcast real transactions and spend funds.\n\n");
    for (k, v) in details {
        body.push_str(&format!("{k}: {v}\n"));
    }
    body.push_str("\nProceed?");
    native_confirm(&format!("Confirm {action}"), body).await
}

/// Money-path run state as a single atomic, so "is a run active?" and "does it
/// observe the cancel token?" can never be observed out of sync.
///
/// Two separate `AtomicBool`s were set in sequence, so `cancel_mint` could see
/// `running == true` with `cancellable == false` and tell the operator a
/// cancellable live run could not be stopped.
mod run_state {
    pub const IDLE: u8 = 0;
    pub const RUNNING: u8 = 1;
    pub const RUNNING_CANCELLABLE: u8 = 2;
}

/// RAII guard for the money-path busy state.
///
/// Releases the run slot on drop (including panic unwind) so the UI cannot
/// stick "busy forever".
struct MintBusyGuard {
    state: Arc<std::sync::atomic::AtomicU8>,
    /// Identifies *this* run so a late `cancel_mint` cannot abort the next one.
    run_id: u64,
}

impl MintBusyGuard {
    /// Claim the run slot in one `compare_exchange`, publishing "running" and
    /// "cancellable?" together so no observer can see a half-built state.
    fn acquire(
        state: &Arc<std::sync::atomic::AtomicU8>,
        run_id_src: &Arc<AtomicU64>,
        cancellable: bool,
    ) -> Result<Self, String> {
        let want = if cancellable {
            run_state::RUNNING_CANCELLABLE
        } else {
            run_state::RUNNING
        };
        state
            .compare_exchange(run_state::IDLE, want, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| String::from(minter_core::mint_busy_message()))?;
        // Bump only after winning the slot: the new id marks this run, and any
        // cancel request carrying an older id is stale and must be ignored.
        let run_id = run_id_src.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Self {
            state: Arc::clone(state),
            run_id,
        })
    }

    /// Acquire for a path that does **not** poll the cancel token, so Stop can
    /// tell the operator the truth instead of claiming it cancelled the run.
    fn try_acquire(
        state: &Arc<std::sync::atomic::AtomicU8>,
        run_id_src: &Arc<AtomicU64>,
    ) -> Result<Self, String> {
        Self::acquire(state, run_id_src, false)
    }

    /// Acquire for a path that honors the cancel token.
    fn try_acquire_cancellable(
        state: &Arc<std::sync::atomic::AtomicU8>,
        run_id_src: &Arc<AtomicU64>,
    ) -> Result<Self, String> {
        Self::acquire(state, run_id_src, true)
    }

    fn run_id(&self) -> u64 {
        self.run_id
    }
}

impl Drop for MintBusyGuard {
    fn drop(&mut self) {
        self.state.store(run_state::IDLE, Ordering::SeqCst);
    }
}

/// Forwards batch (WL check / auth test / …) progress to the webview as
/// `batch-event`, so long multi-wallet runs show rows as they finish instead of
/// nothing until the whole set completes.
struct TauriBatchReporter {
    app: AppHandle,
}

impl minter_core::batch::BatchReporter for TauriBatchReporter {
    fn report(&self, event: minter_core::batch::BatchEvent) {
        let _ = self.app.emit("batch-event", &event);
    }
}

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
    /// Money-path run state: IDLE / RUNNING / RUNNING_CANCELLABLE as one atomic,
    /// so "busy" and "stoppable" are always observed together.
    pub run_state: Arc<std::sync::atomic::AtomicU8>,
    /// Monotonic run counter. `cancel_mint` captures it and only cancels while
    /// it still matches, so a late Stop cannot abort the *next* run.
    pub run_id: Arc<AtomicU64>,
    /// Run id that is currently accepting cancellation (0 = none). Set once the
    /// cancel flag has been armed for that run.
    pub cancel_target: Arc<AtomicU64>,
    /// Shared across reporter for one-shot first confirm per run.
    pub mint_first_confirm: Arc<AtomicBool>,
    /// Cancel token for batch wallet operations (WL check, …).
    pub batch_cancel: minter_core::batch::BatchCancel,
    /// Last UI activity (unix seconds). The idle-lock timer in Rust reads this;
    /// see `spawn_idle_lock_watchdog`.
    pub last_activity: Arc<AtomicU64>,
    /// token → path for files the operator picked in a native dialog.
    ///
    /// `read_text_file` resolves tokens through this map instead of trusting a
    /// path from the webview, so the frontend can only read files the user
    /// actually chose in an OS dialog.
    pub picked_files: Arc<Mutex<std::collections::HashMap<String, std::path::PathBuf>>>,
    /// Counter for file-token uniqueness. Deliberately separate from `run_id`,
    /// which `cancel_mint` compares against and must only move per money-path run.
    pub token_seq: Arc<AtomicU64>,
    /// Serializes UI state-file saves (tasks / wallet_meta / runs_history) so
    /// two debounced writes cannot interleave.
    pub save_lock: Arc<Mutex<()>>,
    /// Caps concurrent network/probe commands.
    ///
    /// Only the money paths took the busy guard, so the webview could launch
    /// unlimited probes/auth calls, each fanning out across every wallet —
    /// exhausting sockets and, worst operationally, tripping an OpenSea rate
    /// limit right before a drop.
    pub net_limit: Arc<tokio::sync::Semaphore>,
    /// Serializes batch wallet runs (WL check), so the single shared
    /// `batch_cancel` token always belongs to exactly one run.
    pub batch_limit: Arc<tokio::sync::Semaphore>,
}

/// Max simultaneous network/probe commands (see [`AppState::net_limit`]).
const MAX_CONCURRENT_NET_CMDS: usize = 4;

/// Acquire a network-command slot, or fail with an operator-readable message.
async fn net_slot(state: &AppState) -> Result<tokio::sync::SemaphorePermit<'_>, String> {
    state.net_limit.try_acquire().map_err(|_| {
        String::from("Too many network checks running — wait for the current ones to finish")
    })
}

impl AppState {
    /// True while any money path holds the run slot.
    fn mint_running(&self) -> bool {
        self.run_state.load(Ordering::SeqCst) != run_state::IDLE
    }

    /// Mark UI activity for the idle-lock watchdog.
    fn touch_activity(&self) {
        self.last_activity.store(now_unix(), Ordering::SeqCst);
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            session: Mutex::new(Session::default_paths()),
            mint_cancel: Arc::new(AtomicBool::new(false)),
            run_state: Arc::new(std::sync::atomic::AtomicU8::new(run_state::IDLE)),
            run_id: Arc::new(AtomicU64::new(0)),
            cancel_target: Arc::new(AtomicU64::new(0)),
            mint_first_confirm: Arc::new(AtomicBool::new(false)),
            batch_cancel: minter_core::batch::BatchCancel::new(),
            last_activity: Arc::new(AtomicU64::new(now_unix())),
            picked_files: Arc::new(Mutex::new(std::collections::HashMap::new())),
            token_seq: Arc::new(AtomicU64::new(0)),
            save_lock: Arc::new(Mutex::new(())),
            net_limit: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_NET_CMDS)),
            batch_limit: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

/// Record a natively-picked path and return the opaque token the UI passes to
/// [`read_text_file`]. Bounded so a UI loop cannot grow the map without limit.
fn register_picked_file(state: &AppState, path: &std::path::Path) -> String {
    let token = format!(
        "f{}-{}",
        now_unix(),
        state.token_seq.fetch_add(1, Ordering::SeqCst)
    );
    let mut map = state.picked_files.lock();
    if map.len() >= 64 {
        map.clear();
    }
    map.insert(token.clone(), path.to_path_buf());
    token
}

/// A file the operator picked: opaque token for reading + display path for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedFile {
    /// Pass to `read_text_file`; not a filesystem path.
    pub token: String,
    /// Full path, for display and for the import commands.
    pub path: String,
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
    /// Configured proxy count — surfaced in the status strip so the operator
    /// can see "0 proxies" before starting a 200-wallet run.
    pub proxy_count: usize,
    pub hint_title: String,
    pub hint_body: String,
}

#[tauri::command]
fn accept_burner(state: State<'_, Arc<AppState>>) {
    state.session.lock().accept_burner_warning();
}

/// Vault ops run 600k-iteration PBKDF2 (up to 3 derivations each). As plain
/// sync commands they executed on the main/event-loop thread and froze the
/// window for seconds — during which the webview cannot paint and no other IPC
/// (including Stop) is processed. Run them on the blocking pool instead.
#[tauri::command]
async fn unlock(state: State<'_, Arc<AppState>>, password: String) -> Result<usize, String> {
    let state = state.inner().clone();
    // Snapshot what the derivation needs, then release the lock. Holding the
    // session mutex across 600k PBKDF2 rounds blocked the 1 Hz `get_status`
    // poll on the main thread, freezing the window for the whole unlock — the
    // very symptom moving to the blocking pool was meant to remove.
    {
        let s = state.session.lock();
        if !s.burner_accepted {
            return Err("Accept burner-wallet warning first".to_string());
        }
    }
    let session_snapshot = state.session.lock().clone();
    let mut unlocked = tauri::async_runtime::spawn_blocking(move || {
        let mut s = session_snapshot;
        // No lock held here: this is the expensive PBKDF2 work.
        s.unlock(&password).map(|n| (s, n))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    // Re-check under the lock and install only the unlocked key material, so a
    // concurrent lock/settings change is not clobbered by the stale snapshot.
    let mut s = state.session.lock();
    if !s.burner_accepted {
        return Err("Accept burner-wallet warning first".to_string());
    }
    s.adopt_unlocked(&mut unlocked.0);
    state.touch_activity();
    Ok(unlocked.1)
}

/// Clear signers + password from RAM (idle lock / manual lock).
///
/// Money paths operate on a `Session` **clone**, so `Session::lock()` cannot
/// reach their copy. Previously this checked "is a run active?" and then took
/// the session mutex as two separate steps: a run starting in between was
/// missed, `lock_vault` cleared only the original, and the UI reported "Locked"
/// while the run kept signing with live keys.
///
/// Holding the session lock across the check closes that window, because every
/// money path clones the session under the same mutex.
#[tauri::command]
fn lock_vault(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let mut s = state.session.lock();
    if state.mint_running() {
        return Err("Cannot lock vault while a mint is running — Stop first".into());
    }
    s.lock();
    Ok("Vault locked".into())
}

/// Frontend heartbeat for the Rust-side idle lock.
///
/// The auto-lock timer used to live entirely in JS, so a compromised or wedged
/// webview simply never locked and the password stayed resident in RAM. Rust
/// now owns the timer; the UI only reports that the operator is present.
#[tauri::command]
fn note_activity(state: State<'_, Arc<AppState>>) {
    state.touch_activity();
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
        proxy_count: s.proxy_count,
        hint_title,
        hint_body,
    }
}

#[tauri::command]
fn list_wallets(state: State<'_, Arc<AppState>>) -> Vec<minter_core::WalletInfo> {
    state.session.lock().list_wallets()
}

#[tauri::command]
async fn add_key(state: State<'_, Arc<AppState>>, private_key: String) -> Result<String, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state
            .session
            .lock()
            .add_key(&private_key)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Largest key list we will read. 16 MB holds ~240k keys — far beyond any real
/// wallet set, while bounding the unbounded `read_to_string` these commands do.
const MAX_KEY_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Most files one `import_files` call may name.
const MAX_IMPORT_FILES: usize = 64;

/// Validate an operator-typed import path.
///
/// The picker path (`token`) is trusted because this process produced it. A typed
/// path is frontend-controlled, so it needs the same care as any other IPC input:
/// unvalidated it was an existence/count oracle, an unbounded read (OOM on a
/// multi-GB file or a device path) and — via UNC — an outbound SMB fetch that
/// leaks the machine's NTLM credentials while bypassing the CSP.
fn validate_key_file_path(raw: &str) -> Result<std::path::PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("No file path given".into());
    }
    // Reject UNC / device namespaces before touching the filesystem: resolving
    // one is itself the network request we are trying to prevent.
    let lowered = trimmed.replace('/', "\\");
    if lowered.starts_with("\\\\") {
        return Err("Network (UNC) paths are not allowed — copy the file locally first".into());
    }

    // Canonicalize so `..` traversal and symlinks resolve before any check.
    let p = std::path::Path::new(trimmed)
        .canonicalize()
        .map_err(|e| format!("Cannot open {trimmed}: {e}"))?;
    // `canonicalize` yields a `\\?\` prefix on Windows; a genuine UNC target
    // becomes `\\?\UNC\server\share`, which must still be refused.
    if p.to_string_lossy().to_ascii_uppercase().contains("\\UNC\\") {
        return Err("Network (UNC) paths are not allowed — copy the file locally first".into());
    }

    let meta = std::fs::metadata(&p).map_err(|e| format!("Cannot read file: {e}"))?;
    if !meta.is_file() {
        return Err("Not a regular file".into());
    }
    if meta.len() > MAX_KEY_FILE_BYTES {
        return Err("File too large (max 16 MB)".into());
    }
    Ok(p)
}

/// Resolve one import source: a picker token (trusted) or a typed path (validated).
fn resolve_import_source(
    state: &AppState,
    token: Option<String>,
    path: Option<String>,
) -> Result<std::path::PathBuf, String> {
    if let Some(tok) = token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let p = state
            .picked_files
            .lock()
            .get(tok)
            .cloned()
            .ok_or_else(|| String::from("Unknown file token — pick the file again"))?;
        // Still bound the size: the operator may have picked a huge file.
        match std::fs::metadata(&p) {
            Ok(m) if m.len() > MAX_KEY_FILE_BYTES => {
                return Err("File too large (max 16 MB)".into())
            }
            Ok(m) if !m.is_file() => return Err("Not a regular file".into()),
            Ok(_) => {}
            Err(e) => return Err(format!("Cannot read file: {e}")),
        }
        return Ok(p);
    }
    validate_key_file_path(path.as_deref().unwrap_or_default())
}

#[tauri::command]
async fn import_file(
    state: State<'_, Arc<AppState>>,
    token: Option<String>,
    path: Option<String>,
) -> Result<usize, String> {
    let resolved = resolve_import_source(&state, token, path)?;
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state
            .session
            .lock()
            .import_file(&resolved)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn import_files(
    state: State<'_, Arc<AppState>>,
    tokens: Option<Vec<String>>,
    paths: Option<Vec<String>>,
) -> Result<usize, String> {
    // Bound the fan-out: an unbounded list multiplied every per-file cost.
    let token_list = tokens.unwrap_or_default();
    let path_list = paths.unwrap_or_default();
    if token_list.len() + path_list.len() > MAX_IMPORT_FILES {
        return Err(format!("Too many files (max {MAX_IMPORT_FILES})"));
    }

    let mut resolved: Vec<std::path::PathBuf> = Vec::new();
    for t in token_list {
        resolved.push(resolve_import_source(&state, Some(t), None)?);
    }
    for p in path_list {
        resolved.push(resolve_import_source(&state, None, Some(p))?);
    }
    if resolved.is_empty() {
        return Err("No files to import".into());
    }

    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state
            .session
            .lock()
            .import_files(&resolved)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn import_keys_text(state: State<'_, Arc<AppState>>, text: String) -> Result<usize, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state
            .session
            .lock()
            .import_keys_text(&text)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
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
    let _slot = net_slot(&state).await?;
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
    let _slot = net_slot(&state).await?;
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
    let _slot = net_slot(&state).await?;
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
    let _slot = net_slot(&state).await?;
    let mut session = {
        let s = state.session.lock();
        s.clone()
    };
    let res = session.probe_rpc().await.map_err(|e| e.to_string())?;
    // Write back ONLY what the probe mutates. Restoring the whole pre-probe
    // clone was a lost update: the probe can run for seconds, and anything the
    // user (or the idle auto-lock) did meanwhile was silently reverted — most
    // dangerously `lock_vault`, which would come back **unlocked** with the
    // private keys reinstated while the UI showed it as locked.
    //
    // `env` is likewise NOT restored wholesale. `apply_settings`/`save_settings`
    // write connection values into `env`, so assigning the stale snapshot back
    // silently reverted a settings save that landed during the probe: the UI
    // showed the new RPC (it lives in `settings`) while requests still used the
    // old one. Only the probe's own status fields are published.
    {
        let mut s = state.session.lock();
        s.rpc_status = session.rpc_status;
        s.network_label = session.network_label;
    }
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
            // Never send the full Alchemy secret into the webview IPC surface.
            // Save still accepts a new key; blank keeps the stored one.
            alchemy_api_key: String::new(),
            alchemy_masked: s.settings.alchemy_masked(),
            rpc_urls: s.settings.rpc_urls.clone(),
            rpc_url_ethereum: s.settings.rpc_url_ethereum.clone(),
            rpc_url_base: s.settings.rpc_url_base.clone(),
            rpc_url_polygon: s.settings.rpc_url_polygon.clone(),
            // Mask `user:pass@` — proxy credentials are paid secrets and must
            // not be shipped to the webview on every settings load. Host:port
            // and line order survive so the editor still round-trips; masked
            // lines are restored on save by `merge_masked_proxy_list`.
            proxy_url: minter_core::proxy::mask_proxy_list(&s.settings.proxy_url),
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
async fn save_settings(
    state: State<'_, Arc<AppState>>,
    input: SaveSettingsInput,
) -> Result<String, String> {
    // Disarming a safety control must be confirmed by the OS, not by the
    // webview: a compromised renderer could otherwise silently turn off the
    // LIVE ceremony for every money path and then spend at will.
    if input.require_live_confirm == Some(false) {
        let currently_on = state.session.lock().settings.require_live_confirm;
        if currently_on {
            native_confirm(
                "Disable LIVE confirmation?",
                "Every live transaction will run WITHOUT the typed-LIVE prompt.\n\n\
                 Only do this if you understand the risk.\n\nDisable it?"
                    .into(),
            )
            .await?;
        }
    }
    if input.dry_run == Some(false) {
        let currently_dry = state.session.lock().dry_run;
        if currently_dry {
            native_confirm(
                "Switch to LIVE mode?",
                "Dry Run will be turned OFF. Runs will broadcast real \
                 transactions and spend real funds.\n\nSwitch to LIVE?"
                    .into(),
            )
            .await?;
        }
    }

    let state = state.inner().clone();
    // Persisting writes config.json (with fsync), mirrors .env and rewrites the
    // proxies file — three blocking writes that must not run on the event loop.
    tauri::async_runtime::spawn_blocking(move || save_settings_inner(&state, input))
        .await
        .map_err(|e| e.to_string())?
}

fn save_settings_inner(state: &AppState, input: SaveSettingsInput) -> Result<String, String> {
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
        // The UI only ever saw masked credentials, so restore any still-masked
        // line from the stored list before persisting. Without this, saving the
        // Settings page would overwrite real credentials with "••••".
        let merged = minter_core::proxy::merge_masked_proxy_list(&v, &settings.proxy_url);
        settings.set_proxies_text(&merged);
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
    let _busy = MintBusyGuard::try_acquire(&state.run_state, &state.run_id)?;
    let dry_run = input.dry_run.unwrap_or(true);
    confirm_live_spend(
        dry_run,
        "sweep ETH",
        &[
            ("Chain", input.chain.clone()),
            ("Send ALL ETH to", input.destination.clone()),
        ],
    )
    .await?;
    let session = state.session.lock().clone();
    session
        .sweep_eth(
            &input.chain,
            &input.destination,
            dry_run,
            input.confirm.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| e.to_string())
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
    let _busy = MintBusyGuard::try_acquire(&state.run_state, &state.run_id)?;
    let dry_run = input.dry_run.unwrap_or(true);
    confirm_live_spend(
        dry_run,
        "sweep NFTs",
        &[
            ("Chain", input.chain.clone()),
            ("Collection", input.contract.clone()),
            ("Transfer ALL NFTs to", input.destination.clone()),
        ],
    )
    .await?;
    let session = state.session.lock().clone();
    session
        .sweep_nfts(
            &input.chain,
            &input.contract,
            &input.destination,
            dry_run,
            input.confirm.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| e.to_string())
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
    let busy = MintBusyGuard::try_acquire_cancellable(&state.run_state, &state.run_id)?;
    let session = state.session.lock().clone();
    let dry_run = input.dry_run.unwrap_or(session.dry_run);
    confirm_live_spend(
        dry_run,
        "live mint",
        &[
            ("Collection", input.slug.clone()),
            (
                "Quantity per wallet",
                input.quantity.unwrap_or(1).max(1).to_string(),
            ),
            (
                "Wallets",
                match input.wallet_addresses.as_ref() {
                    Some(v) if !v.is_empty() => v.len().to_string(),
                    _ => format!("all ({})", session.signers.len()),
                },
            ),
        ],
    )
    .await?;
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
    // Typed LIVE — enforced in core when require_live_confirm && !dry_run.
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
    // Publish the run id only after the cancel flag is cleared, so a Stop that
    // arrives from here on is matched against this run and not swallowed.
    state.cancel_target.store(busy.run_id(), Ordering::SeqCst);
    session
        .run_opensea_mint_cancellable(opts, &confirm, reporter, cancel)
        .await
        .map_err(|e| {
            let s = e.to_string();
            let lower = s.to_lowercase();
            if lower.contains("chain")
                && (lower.contains("mismatch") || lower.contains("wrong network"))
            {
                // Keep the original error — it names the actual chains, which
                // is the one detail the user needs to fix the run. The canned
                // message rendered literally as "collection expects
                // 'collection', RPC reports 'RPC'".
                format!(
                    "{s} — open Settings → Connection and set an RPC / Alchemy network matching the collection"
                )
            } else if lower.contains("401") || lower.contains("unauthorized") {
                format!("{} ({s})", minter_core::reauth_required_message())
            } else {
                s
            }
        })
}

#[tauri::command]
fn cancel_mint(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    // Single load: "running?" and "cancellable?" are one atomic, so Stop can no
    // longer observe a half-published state and wrongly report a cancellable
    // run as unstoppable.
    let st = state.run_state.load(Ordering::SeqCst);
    if st == run_state::IDLE {
        return Ok("No mint running".into());
    }
    // Only some money paths poll the cancel token. Reporting "cancel requested"
    // for the others (raw_mint, sweeps, disperse, multicall) told the operator
    // their funds were held back while the sends carried on to completion.
    if st != run_state::RUNNING_CANCELLABLE {
        return Err("This operation cannot be stopped mid-run — it will finish on its own.".into());
    }
    // Bind the request to the run that is live *now*. Without this, a Stop for
    // run A that lands just after A finished cancelled the freshly started B.
    let target = state.cancel_target.load(Ordering::SeqCst);
    if target == 0 || target != state.run_id.load(Ordering::SeqCst) {
        return Ok("No mint running".into());
    }
    state.mint_cancel.store(true, Ordering::SeqCst);
    Ok("Stopping… cancel requested".into())
}

#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Resolve a runtime output directory, creating it and reporting real failures.
///
/// Deliberately cwd-relative: `minter_core::export` writes to a bare
/// `results/` path (`export.rs:58`), so anchoring the desktop side to
/// `app_data_dir` would make "Open results folder" show an always-empty
/// directory while the exports landed elsewhere. Both sides must agree, so the
/// location stays and only the swallowed error is fixed.
fn runtime_dir(name: &str) -> Result<std::path::PathBuf, String> {
    let p = std::env::current_dir()
        .map_err(|e| format!("cannot resolve working directory: {e}"))?
        .join(name);
    // Was `let _ = create_dir_all(...)`: on a read-only or non-writable cwd the
    // path was still returned as if valid, and the operator only found out when
    // an export silently failed later.
    std::fs::create_dir_all(&p).map_err(|e| format!("cannot create {}: {e}", p.display()))?;
    Ok(p)
}

#[tauri::command]
fn results_dir() -> Result<String, String> {
    Ok(runtime_dir("results")?.display().to_string())
}

#[tauri::command]
fn open_results_folder() -> Result<String, String> {
    let p = runtime_dir("results")?;
    open_folder(&p)?;
    Ok(p.display().to_string())
}

fn open_folder(p: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // Absolute path from %SystemRoot%: `Command::new("explorer")` resolves
        // through the application directory and the cwd before PATH, so an
        // `explorer.exe` dropped next to the binary would be executed instead.
        let exe = std::env::var("SystemRoot")
            .map(|r| std::path::PathBuf::from(r).join("explorer.exe"))
            .unwrap_or_else(|_| std::path::PathBuf::from(r"C:\Windows\explorer.exe"));
        std::process::Command::new(exe)
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
    let p = runtime_dir("logs")?;
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
    state.mint_running()
}

#[tauri::command]
async fn test_auth(
    state: State<'_, Arc<AppState>>,
    all_wallets: bool,
) -> Result<Vec<minter_core::AuthTestRow>, String> {
    let _slot = net_slot(&state).await?;
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
    let _slot = net_slot(&state).await?;
    let session = state.session.lock().clone();
    let addrs = input.and_then(|i| i.wallet_addresses);
    session.warm_auth(addrs).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn check_eligibility(
    state: State<'_, Arc<AppState>>,
    slug: String,
) -> Result<minter_core::EligibilityResult, String> {
    let _slot = net_slot(&state).await?;
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
    /// Operator-selected worker count (forced to 1 when no proxies are set).
    concurrency: Option<usize>,
}

/// Multi-wallet WL / eligibility (proxies by vault index).
///
/// Takes a network slot so N concurrent runs cannot be launched. That also fixes
/// a shared-token bug: `batch_cancel.reset()` on a second run silently
/// **un-cancelled** the first one the operator had just stopped, and both runs
/// emitted onto the same `batch-event` channel, interleaving rows from different
/// runs into one table.
#[tauri::command]
async fn check_eligibility_wallets(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    input: CheckEligibilityWalletsInput,
) -> Result<minter_core::WalletEligibilityReport, String> {
    // Serialize batch runs: only one may hold this slot at a time.
    let _batch = state
        .batch_limit
        .try_acquire()
        .map_err(|_| String::from("A wallet check is already running — Stop it first"))?;
    let _slot = net_slot(&state).await?;

    let session = state.session.lock().clone();
    let wallets = input.wallet_addresses.and_then(|v| {
        let v: Vec<String> = v.into_iter().filter(|s| !s.trim().is_empty()).collect();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    });
    let reporter: Arc<dyn minter_core::batch::BatchReporter> = Arc::new(TauriBatchReporter { app });
    // Safe to reset now: the slot above guarantees no other run is in flight, so
    // this cannot revive a run the operator just cancelled.
    state.batch_cancel.reset();
    let cancel = state.batch_cancel.clone();
    session
        .check_eligibility_wallets_streaming(
            &input.slug,
            wallets,
            input.concurrency,
            Some(reporter),
            Some(cancel),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Stop an in-flight batch run. Rows already checked are kept and exported.
#[tauri::command]
fn cancel_batch(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    // No run holding the slot means there is nothing to stop; say so instead of
    // arming a flag that the next run would inherit.
    if state.batch_limit.available_permits() > 0 {
        return Ok("No wallet check running".into());
    }
    state.batch_cancel.cancel();
    Ok("Stopping… finishing in-flight wallets".into())
}

#[tauri::command]
async fn measure_latency(
    state: State<'_, Arc<AppState>>,
) -> Result<minter_core::LatencyReport, String> {
    let _slot = net_slot(&state).await?;
    let session = state.session.lock().clone();
    session.measure_latency().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn discover_raw_functions(
    state: State<'_, Arc<AppState>>,
    contract: String,
    chain: String,
) -> Result<Vec<minter_core::DiscoveredFunction>, String> {
    let _slot = net_slot(&state).await?;
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
    let _busy = MintBusyGuard::try_acquire(&state.run_state, &state.run_id)?;
    let dry_run = input.dry_run.unwrap_or(true);
    // Arbitrary contract + calldata + value: the highest-risk command in the
    // app. Show exactly what will be signed, from the OS, not the webview.
    confirm_live_spend(
        dry_run,
        "raw contract call",
        &[
            ("Chain", input.chain.clone()),
            ("Contract", input.contract.clone()),
            ("Function", input.function.clone()),
            (
                "Value per wallet (ETH)",
                input.value_eth.clone().unwrap_or_else(|| "0".into()),
            ),
        ],
    )
    .await?;
    let session = state.session.lock().clone();
    let gas_mult = input
        .gas_multiplier
        .as_deref()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|m| *m > 0.0);
    session
        .raw_mint(
            &input.chain,
            &input.contract,
            &input.function,
            input.params.unwrap_or_default(),
            input.value_eth.as_deref().unwrap_or("0"),
            dry_run,
            input.confirm.as_deref().unwrap_or(""),
            input.wallet_addresses,
            input.use_flashbots.unwrap_or(false),
            input.priority_fee_gwei.as_deref(),
            input.max_fee_gwei.as_deref(),
            gas_mult,
            input.gas_limit,
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn raw_sniper(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    input: minter_core::RawSniperInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    let busy = MintBusyGuard::try_acquire_cancellable(&state.run_state, &state.run_id)?;
    let dry_run = input.dry_run.unwrap_or(true);
    // Pre-signed transactions cannot be recalled once the clock fires, so this
    // must be confirmed before any signing happens.
    confirm_live_spend(
        dry_run,
        "raw sniper",
        &[
            ("Chain", input.chain.clone()),
            ("Contract", input.contract.clone()),
            (
                "Value per wallet (ETH)",
                input.value_eth.clone().unwrap_or_else(|| "0".into()),
            ),
            (
                "Fires at",
                input
                    .at_time
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "immediately".into()),
            ),
        ],
    )
    .await?;
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
    state.cancel_target.store(busy.run_id(), Ordering::SeqCst);
    session
        .raw_sniper(input, cancel, Some(reporter))
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisperseInput {
    chain: String,
    from_address: String,
    to_addresses: Vec<String>,
    amount_eth: String,
    dry_run: Option<bool>,
    confirm: Option<String>,
}

#[tauri::command]
async fn disperse(
    state: State<'_, Arc<AppState>>,
    input: DisperseInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    let _busy = MintBusyGuard::try_acquire(&state.run_state, &state.run_id)?;
    let dry_run = input.dry_run.unwrap_or(true);
    confirm_live_spend(
        dry_run,
        "disperse ETH",
        &[
            ("Chain", input.chain.clone()),
            ("From", input.from_address.clone()),
            ("Recipients", input.to_addresses.len().to_string()),
            ("Amount each (ETH)", input.amount_eth.clone()),
        ],
    )
    .await?;
    let session = state.session.lock().clone();
    session
        .disperse(
            &input.chain,
            &input.from_address,
            input.to_addresses,
            &input.amount_eth,
            dry_run,
            input.confirm.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MulticallInput {
    chain: String,
    from_address: String,
    steps: Vec<minter_core::MulticallStepInput>,
    dry_run: Option<bool>,
    multicall_address: Option<String>,
    confirm: Option<String>,
}

#[tauri::command]
async fn multicall(
    state: State<'_, Arc<AppState>>,
    input: MulticallInput,
) -> Result<Vec<minter_core::SweepResultRow>, String> {
    let _busy = MintBusyGuard::try_acquire(&state.run_state, &state.run_id)?;
    let dry_run = input.dry_run.unwrap_or(true);
    confirm_live_spend(
        dry_run,
        "multicall",
        &[
            ("Chain", input.chain.clone()),
            ("From", input.from_address.clone()),
            ("Steps", input.steps.len().to_string()),
        ],
    )
    .await?;
    let session = state.session.lock().clone();
    session
        .multicall(
            &input.chain,
            &input.from_address,
            input.steps,
            dry_run,
            input.multicall_address,
            input.confirm.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| e.to_string())
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
async fn remove_wallet(state: State<'_, Arc<AppState>>, address: String) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state
            .session
            .lock()
            .remove_wallet(&address)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Toggle Dry Run / Live in session + persist to config.json.
#[tauri::command]
async fn set_dry_run(state: State<'_, Arc<AppState>>, dry_run: bool) -> Result<bool, String> {
    // Arming LIVE is a security-relevant transition — confirm natively so the
    // webview cannot flip it silently.
    if !dry_run {
        let currently_dry = state.session.lock().dry_run;
        if currently_dry {
            native_confirm(
                "Switch to LIVE mode?",
                "Dry Run will be turned OFF. Runs will broadcast real \
                 transactions and spend real funds.\n\nSwitch to LIVE?"
                    .into(),
            )
            .await?;
        }
    }
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut s = state.session.lock();
        s.dry_run = dry_run;
        s.settings.dry_run = dry_run;
        s.save_settings().map_err(|e| e.to_string())?;
        Ok(s.dry_run)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Native file picker (keys / proxy list).
///
/// `rfd` pumps a modal message loop, so it must not run on the Tauri event loop
/// thread — that froze the window for as long as the dialog was open.
#[tauri::command]
async fn pick_file(
    state: State<'_, Arc<AppState>>,
    title: Option<String>,
    filters: Option<Vec<String>>,
) -> Result<Option<PickedFile>, String> {
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut d = rfd::FileDialog::new().set_title(title.as_deref().unwrap_or("Open file"));
        let exts: Vec<String> = filters.unwrap_or_else(|| vec!["txt".into(), "*".into()]);
        let refs: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
        if !refs.is_empty() {
            d = d.add_filter("Text / list", &refs);
        }
        d.pick_file()
    })
    .await
    .map_err(|e| e.to_string())?;

    Ok(picked.map(|p| PickedFile {
        token: register_picked_file(&state, &p),
        path: p.display().to_string(),
    }))
}

/// Multi-file picker (import several key lists at once).
#[tauri::command]
async fn pick_files(
    state: State<'_, Arc<AppState>>,
    title: Option<String>,
    filters: Option<Vec<String>>,
) -> Result<Vec<PickedFile>, String> {
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut d = rfd::FileDialog::new().set_title(title.as_deref().unwrap_or("Open files"));
        let exts: Vec<String> =
            filters.unwrap_or_else(|| vec!["txt".into(), "csv".into(), "*".into()]);
        let refs: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
        if !refs.is_empty() {
            d = d.add_filter("Text / list", &refs);
        }
        d.pick_files().unwrap_or_default()
    })
    .await
    .map_err(|e| e.to_string())?;

    Ok(picked
        .into_iter()
        .map(|p| PickedFile {
            token: register_picked_file(&state, &p),
            path: p.display().to_string(),
        })
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

/// Durably replace `path` with `data`: unique temp → write → fsync → rename,
/// then fsync the parent directory.
///
/// Replaces three copies of a temp-then-rename routine that each had the same
/// defects: no `fsync` (so a power cut could promote a zero-length file over
/// good data), a fallback that reopened the *real* file with `O_TRUNC` and
/// therefore destroyed it on a transient rename failure, a swallowed rename
/// error, a leaked temp on the error path, and a fixed temp name that two
/// concurrent debounced saves raced on.
fn atomic_write_json(path: &std::path::Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write as _;

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
    }

    // Unique per process + call, so concurrent saves cannot share a temp file.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = std::path::PathBuf::from(tmp);

    // Scope the handle so it is closed before the rename (required on Windows).
    let write_res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        // Durability: without this the rename metadata can reach disk before the
        // file contents, leaving a truncated file in place of the good one.
        f.sync_all()
    })();
    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", tmp.display()));
    }

    // Windows `rename` cannot overwrite, so prefer the replacing form and fall
    // back to remove+rename. Never truncate the destination directly.
    let renamed = std::fs::rename(&tmp, path).or_else(|first| {
        if path.exists() {
            std::fs::remove_file(path).and_then(|_| std::fs::rename(&tmp, path))
        } else {
            Err(first)
        }
    });
    if let Err(e) = renamed {
        // Surface the error instead of silently corrupting the live file, and do
        // not leave the temp behind.
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("replace {}: {e}", path.display()));
    }

    // Make the rename itself durable.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Serialize + durably persist a UI state file, rejecting oversized payloads.
///
/// The webview supplies these documents, so an unbounded `Vec<Value>` could be
/// grown until the disk filled.
fn save_json_file<T: Serialize>(path: &std::path::Path, value: &T) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    const MAX_BYTES: usize = 16 * 1024 * 1024;
    if json.len() > MAX_BYTES {
        return Err(format!(
            "refusing to save {}: payload {} bytes exceeds 16 MB cap",
            path.display(),
            json.len()
        ));
    }
    atomic_write_json(path, json.as_bytes())
}

/// Preserve a corrupt data file before the caller falls back to an empty value.
///
/// The loaders return `Ok(empty)` so the app still starts, but the UI persists
/// on the next mutation — which would overwrite the damaged-yet-recoverable
/// file with the empty set. Copying it aside first keeps the data recoverable.
fn backup_corrupt_file(path: &std::path::Path, what: &str, err: &impl std::fmt::Display) {
    // Timestamped: a fixed `.json.corrupt` name meant a second corruption event
    // silently overwrote the first backup — potentially replacing recoverable
    // data with an already-damaged copy.
    let backup = path.with_extension(format!("json.corrupt.{}", now_unix()));
    match std::fs::copy(path, &backup) {
        Ok(_) => eprintln!(
            "{what} load error: {err} — original preserved at {}",
            backup.display()
        ),
        Err(copy_err) => {
            eprintln!("{what} load error: {err} (backup failed: {copy_err})")
        }
    }
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
            backup_corrupt_file(&path, "wallet_meta.json", &e);
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
    // Serialize saves so two debounced writes cannot interleave.
    let _w = state.save_lock.lock();
    save_json_file(&path, &out)
}

/// Read a text file the operator picked in the native dialog.
///
/// Takes an opaque **token**, never a path. The previous signature accepted any
/// path from the webview, which made this an arbitrary local file read: the
/// extension allowlist still permitted every `.txt`/`.csv` on the machine (seed
/// phrases, exported password CSVs), the `PROTECTED` filename list was dead code
/// (all its entries were already rejected by the extension check) and could be
/// bypassed with a hardlink, and UNC paths turned it into an SMB fetch that
/// leaked NTLM credentials while bypassing the CSP.
///
/// Now only a path this process itself returned from `pick_file`/`pick_files`
/// can be read, so a compromised renderer cannot name a target at all.
#[tauri::command]
async fn read_text_file(state: State<'_, Arc<AppState>>, token: String) -> Result<String, String> {
    let path = state
        .picked_files
        .lock()
        .get(&token)
        .cloned()
        .ok_or_else(|| String::from("Unknown file token — pick the file again"))?;

    tauri::async_runtime::spawn_blocking(move || {
        // Open once, then stat and read through that same handle, so the file
        // cannot be swapped between the size check and the read (TOCTOU).
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        const MAX: u64 = 8 * 1024 * 1024;
        if len > MAX {
            return Err("File too large (max 8 MB)".into());
        }
        use std::io::Read as _;
        let mut buf = String::new();
        file.take(MAX)
            .read_to_string(&mut buf)
            .map_err(|_| String::from("File is not valid UTF-8 text"))?;
        Ok(buf)
    })
    .await
    .map_err(|e| e.to_string())?
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
            // Corrupt file: return empty, but keep a copy so the next
            // debounced save cannot destroy it.
            backup_corrupt_file(&path, "tasks.json", &e);
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
    let _w = state.save_lock.lock();
    save_json_file(&path, &out)
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
            backup_corrupt_file(&path, "runs_history.json", &e);
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
    let _w = state.save_lock.lock();
    save_json_file(&path, &out)
}

#[tauri::command]
async fn list_drop_phases(
    state: State<'_, Arc<AppState>>,
    slug: String,
) -> Result<minter_core::DropPhasesResult, String> {
    let _slot = net_slot(&state).await?;
    let session = state.session.lock().clone();
    session
        .list_drop_phases(&slug)
        .await
        .map_err(|e| e.to_string())
}

/// Enforce `idle_lock_minutes` in Rust.
///
/// The auto-lock used to be a `setTimeout` in the webview, i.e. a security
/// control on the untrusted side of the boundary: a compromised or wedged
/// renderer simply never locked, and the vault password plus every decrypted
/// signer stayed resident in RAM across screen-lock, sleep and hibernation.
/// The UI now only reports activity (`note_activity`); the timer lives here.
fn spawn_idle_lock_watchdog(state: Arc<AppState>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(15));

            let minutes = {
                let s = state.session.lock();
                if !s.is_unlocked() {
                    continue;
                }
                s.settings.idle_lock_minutes
            };
            // 0 disables auto-lock (explicit operator choice).
            if minutes == 0 {
                continue;
            }
            let idle = now_unix().saturating_sub(state.last_activity.load(Ordering::SeqCst));
            if idle >= (minutes as u64).saturating_mul(60) {
                // Take the session lock BEFORE re-checking the run state, and
                // hold it across the clear. Money paths clone the session under
                // this same mutex, so a run cannot slip in between the check and
                // the lock and end up holding keys the UI reports as locked.
                let mut s = state.session.lock();
                if state.mint_running() {
                    // Never yank keys out from under an in-flight money path.
                    drop(s);
                    state.touch_activity();
                    continue;
                }
                if !s.is_unlocked() {
                    continue;
                }
                s.lock();
                drop(s);
                state.touch_activity();
                eprintln!("vault auto-locked after {minutes} min idle");
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Desktop: quiet core logs unless user set QUIET/DEBUG explicitly.
    if std::env::var("QUIET").is_err() && std::env::var("DEBUG").is_err() {
        // SAFETY: single-threaded before async runtime serves UI
        unsafe { std::env::set_var("QUIET", "1") };
    }
    let state = Arc::new(AppState::default());
    spawn_idle_lock_watchdog(state.clone());
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            accept_burner,
            unlock,
            lock_vault,
            note_activity,
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
            cancel_batch,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(label: &str) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "minter_desktop_test_{}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            label
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atomic_write_creates_and_leaves_no_temp() {
        let dir = tmp_dir("create");
        let p = dir.join("tasks.json");
        atomic_write_json(&p, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"a\":1}");
        // No stray temp files left behind.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(strays.is_empty(), "temp file leaked: {strays:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_replaces_existing_without_truncating_on_failure() {
        let dir = tmp_dir("replace");
        let p = dir.join("tasks.json");
        atomic_write_json(&p, b"OLD").unwrap();
        atomic_write_json(&p, b"NEWER-AND-LONGER").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"NEWER-AND-LONGER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_creates_missing_parent() {
        let dir = tmp_dir("nested");
        let p = dir.join("sub").join("deeper").join("f.json");
        atomic_write_json(&p, b"{}").unwrap();
        assert!(p.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_json_file_rejects_oversized_payload() {
        let dir = tmp_dir("cap");
        let p = dir.join("tasks.json");
        // The webview supplies this document; it must not be able to fill the disk.
        // ~64 bytes of payload per entry × 400k ≈ 25 MB serialized.
        let filler = "p".repeat(64);
        let huge: Vec<String> = (0..400_000).map(|_| filler.clone()).collect();
        let err = save_json_file(&p, &huge).unwrap_err();
        assert!(err.contains("16 MB"), "{err}");
        assert!(!p.exists(), "oversized payload must not be written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn busy_guard_is_mutually_exclusive_and_releases_on_drop() {
        let st = Arc::new(std::sync::atomic::AtomicU8::new(run_state::IDLE));
        let id = Arc::new(AtomicU64::new(0));

        let g = MintBusyGuard::try_acquire(&st, &id).unwrap();
        assert!(MintBusyGuard::try_acquire(&st, &id).is_err());
        assert!(MintBusyGuard::try_acquire_cancellable(&st, &id).is_err());
        drop(g);
        assert_eq!(st.load(Ordering::SeqCst), run_state::IDLE);
        // Slot is reusable after release.
        assert!(MintBusyGuard::try_acquire(&st, &id).is_ok());
    }

    #[test]
    fn busy_guard_publishes_running_and_cancellable_together() {
        let st = Arc::new(std::sync::atomic::AtomicU8::new(run_state::IDLE));
        let id = Arc::new(AtomicU64::new(0));

        // Non-cancellable path: Stop must be able to tell the operator the truth.
        let g = MintBusyGuard::try_acquire(&st, &id).unwrap();
        assert_eq!(st.load(Ordering::SeqCst), run_state::RUNNING);
        drop(g);

        // Cancellable path: never observable as "running but not cancellable".
        let g = MintBusyGuard::try_acquire_cancellable(&st, &id).unwrap();
        assert_eq!(st.load(Ordering::SeqCst), run_state::RUNNING_CANCELLABLE);
        drop(g);
    }

    #[test]
    fn run_ids_are_unique_and_monotonic() {
        let st = Arc::new(std::sync::atomic::AtomicU8::new(run_state::IDLE));
        let id = Arc::new(AtomicU64::new(0));

        let a = MintBusyGuard::try_acquire_cancellable(&st, &id).unwrap();
        let first = a.run_id();
        drop(a);
        let b = MintBusyGuard::try_acquire_cancellable(&st, &id).unwrap();
        // A Stop captured for run A must not match run B.
        assert!(b.run_id() > first, "run id must advance per run");
    }

    #[test]
    fn busy_flag_clears_after_panic_unwind() {
        let st = Arc::new(std::sync::atomic::AtomicU8::new(run_state::IDLE));
        let id = Arc::new(AtomicU64::new(0));
        let st2 = Arc::clone(&st);
        let id2 = Arc::clone(&id);
        let res = std::panic::catch_unwind(move || {
            let _g = MintBusyGuard::try_acquire(&st2, &id2).unwrap();
            panic!("boom");
        });
        assert!(res.is_err());
        assert_eq!(
            st.load(Ordering::SeqCst),
            run_state::IDLE,
            "guard must release the slot on unwind, not stick 'busy forever'"
        );
    }

    #[test]
    fn picked_file_tokens_are_unique_and_resolve() {
        let state = AppState::default();
        let t1 = register_picked_file(&state, std::path::Path::new("a.txt"));
        let t2 = register_picked_file(&state, std::path::Path::new("b.txt"));
        assert_ne!(t1, t2);
        let map = state.picked_files.lock();
        assert_eq!(map.get(&t1).unwrap(), std::path::Path::new("a.txt"));
        assert_eq!(map.get(&t2).unwrap(), std::path::Path::new("b.txt"));
    }

    #[test]
    fn typed_import_path_rejects_unc() {
        // A UNC path is an outbound SMB fetch: it leaks NTLM credentials and
        // bypasses the CSP entirely. Must be refused before touching the FS.
        for p in [
            r"\\attacker.tld\share\keys.txt",
            r"//attacker.tld/share/keys.txt",
        ] {
            let err = validate_key_file_path(p).unwrap_err();
            assert!(err.contains("UNC"), "{p} => {err}");
        }
    }

    #[test]
    fn typed_import_path_rejects_blank_and_missing() {
        assert!(validate_key_file_path("").is_err());
        assert!(validate_key_file_path("   ").is_err());
        assert!(validate_key_file_path(r"Z:\definitely\missing\keys.txt").is_err());
    }

    #[test]
    fn typed_import_path_rejects_directory_and_accepts_file() {
        let dir = tmp_dir("import");
        // A directory is not a key list.
        assert!(validate_key_file_path(&dir.display().to_string()).is_err());

        let f = dir.join("keys.txt");
        std::fs::write(&f, b"0xabc\n").unwrap();
        let ok = validate_key_file_path(&f.display().to_string()).unwrap();
        assert!(ok.is_absolute(), "path must be canonicalized");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typed_import_path_rejects_oversized_file() {
        let dir = tmp_dir("import_big");
        let f = dir.join("big.txt");
        std::fs::write(&f, vec![b'a'; (MAX_KEY_FILE_BYTES + 1) as usize]).unwrap();
        let err = validate_key_file_path(&f.display().to_string()).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typed_import_path_resolves_traversal() {
        // `..` must resolve before validation, not be taken at face value.
        let dir = tmp_dir("traverse");
        let f = dir.join("keys.txt");
        std::fs::write(&f, b"0xabc\n").unwrap();
        let sneaky = dir.join("sub").join("..").join("keys.txt");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let ok = validate_key_file_path(&sneaky.display().to_string()).unwrap();
        assert!(ok.ends_with("keys.txt"));
        assert!(!ok.to_string_lossy().contains(".."), "{ok:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_source_prefers_token_and_rejects_unknown() {
        let state = AppState::default();
        let dir = tmp_dir("src");
        let f = dir.join("keys.txt");
        std::fs::write(&f, b"0xabc\n").unwrap();

        let tok = register_picked_file(&state, &f);
        let got = resolve_import_source(&state, Some(tok), None).unwrap();
        assert_eq!(got, f);

        // An invented token must not fall through to any path handling.
        let err = resolve_import_source(&state, Some("bogus".into()), None).unwrap_err();
        assert!(err.contains("Unknown file token"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_limit_serializes_runs() {
        let state = AppState::default();
        let a = state.batch_limit.try_acquire().unwrap();
        // A second WL run must be refused while the first holds the slot, so
        // `batch_cancel.reset()` cannot revive a just-cancelled run.
        assert!(state.batch_limit.try_acquire().is_err());
        drop(a);
        assert!(state.batch_limit.try_acquire().is_ok());
    }

    #[test]
    fn net_limit_caps_concurrent_probes() {
        let state = AppState::default();
        let held: Vec<_> = (0..MAX_CONCURRENT_NET_CMDS)
            .map(|_| state.net_limit.try_acquire().unwrap())
            .collect();
        assert!(
            state.net_limit.try_acquire().is_err(),
            "must cap at {MAX_CONCURRENT_NET_CMDS}"
        );
        drop(held);
        assert!(state.net_limit.try_acquire().is_ok());
    }

    #[test]
    fn picked_file_registry_is_bounded() {
        let state = AppState::default();
        for i in 0..200 {
            register_picked_file(&state, std::path::Path::new(&format!("f{i}.txt")));
        }
        assert!(
            state.picked_files.lock().len() <= 64,
            "registry must not grow without bound"
        );
    }
}
