//! OpenSea mint orchestration (auth, phase, calldata, send, RBF).
//! Shared by CLI and desktop via `MintReporter` + `MintOptions`.

use alloy_primitives::{Address, Bytes, B256, U256};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::api::{collect_rpc_urls_for_chain, parse_collection_slug, MintOptions};
use crate::auth_cache;
use crate::export;
use crate::flashbots::{self, BundleTx, FlashbotsClient, FlashbotsConfig, MAINNET_CHAIN_ID};
use crate::gas;
use crate::opensea;
use crate::progress::{FileTeeReporter, MintEvent, MintReporter, NullReporter};
use crate::proxy::ProxyManager;
use crate::rpc;
use crate::sign;
use crate::types::*;

fn report_msg(reporter: &dyn MintReporter, quiet: bool, msg: impl Into<String>) {
    if !quiet {
        reporter.report(MintEvent::message(msg));
    }
}

fn log_always(reporter: &dyn MintReporter, msg: impl Into<String>) {
    reporter.report(MintEvent::message(msg));
}

fn report_phase(reporter: &dyn MintReporter, phase: &str, label: impl Into<String>) {
    let label = label.into();
    reporter.report(MintEvent::phase(phase, label.clone()));
    // Also mirror into log stream so file/UI log stays complete
    log_always(reporter, format!("[{}] {}", phase.to_uppercase(), label));
}

fn report_wallet(
    reporter: &dyn MintReporter,
    address: &Address,
    status: Option<WalletStatus>,
    detail: Option<String>,
    tx_hash: Option<B256>,
    error: Option<String>,
) {
    reporter.report(MintEvent::wallet(*address, status, detail, tx_hash, error));
}

fn mint_log(reporter: &dyn MintReporter, quiet: bool, msg: impl AsRef<str>) {
    report_msg(reporter, quiet, msg.as_ref().to_string());
}

/// Summary returned after a mint run (for UI / export).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MintRunSummary {
    pub slug: String,
    pub chain: String,
    pub phase: String,
    pub dry_run: bool,
    pub elapsed_ms: u64,
    #[serde(skip)]
    pub results: Vec<MintResult>,
    pub confirmed: usize,
    pub failed: usize,
    pub export_json: Option<String>,
    pub export_csv: Option<String>,
    /// Per-wallet rows for UI table (address, status, tx, error).
    pub wallets: Vec<crate::api::SweepResultRow>,
}

fn maybe_beep(beep: bool, first_confirm: &AtomicBool) {
    if !beep {
        return;
    }
    if first_confirm
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        // CLI/terminal only. Desktop plays a real system chime in TauriMintReporter.
        print!("\x07");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

// Error classification lives in one place (`crate::errors`) so retry / RBF
// decisions stay consistent across providers and are unit-tested there.
pub(crate) use crate::errors::classify_mint_error;
use crate::errors::{is_already_known, is_nonce_too_low, is_underpriced};

fn parse_hex_u256(value: &str) -> Option<U256> {
    if let Some(hex) = value.strip_prefix("0x") {
        U256::from_str_radix(hex, 16).ok()
    } else {
        U256::from_str_radix(value, 10).ok()
    }
}


struct WalletAuth {
    address: alloy_primitives::Address,
    signer: Signer,
    session: Option<opensea::AuthSession>,
    auth_ok: bool,
    nonce: u64,
    prefetched_tx: Option<(alloy_primitives::Address, U256, Bytes)>,
    /// Proxy assigned at auth time (signer index). Must not be re-derived from
    /// `wallets` order — cache vs join reorders the vec.
    proxy_url: Option<String>,
}

const DEFAULT_SEADROP_ADDRESS: &str = "0x00005EA00Ac477B1030CE78506496e8C2dE24bf5";

/// Decode OpenSea / local mint `data` hex. Fail-fast on empty or invalid.
pub(crate) fn parse_tx_calldata_hex(data_hex: &str) -> anyhow::Result<Bytes> {
    let raw = data_hex.trim();
    if raw.is_empty() || raw == "0x" || raw == "0X" {
        bail!("empty calldata");
    }
    let hex_body = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")).unwrap_or(raw);
    if hex_body.is_empty() {
        bail!("empty calldata");
    }
    let bytes = hex::decode(hex_body).context("invalid calldata hex")?;
    if bytes.is_empty() {
        bail!("empty calldata");
    }
    Ok(Bytes::from(bytes))
}

/// Wall-clock fire lag in ms: `now_ms − start_ts*1000`, floored at 0.
pub(crate) fn fire_lag_ms_from_clock(start_ts: i64, now_ms: i64) -> u64 {
    let open_ms = start_ts.saturating_mul(1000);
    now_ms.saturating_sub(open_ms).max(0) as u64
}

/// Resolve gas limit for OpenSea mint: estimate path uses L2 floors; fixed path
/// clamps up on elevated chains when operator fixed is below floor.
///
/// When `is_fixed` is true, `estimated_or_fixed` is a hard fixed limit (still L2-clamped).
pub(crate) fn resolve_mint_gas_limit(
    estimated_or_fixed: u64,
    gas_multiplier: f64,
    chain_id: u64,
    is_fixed: bool,
) -> u64 {
    if is_fixed {
        let mut limit = estimated_or_fixed.max(21_000);
        if gas::chain_needs_elevated_gas(chain_id) {
            const L2_FLOOR: u64 = 150_000;
            if limit < L2_FLOOR {
                limit = L2_FLOOR;
            }
        }
        limit.min(15_000_000)
    } else {
        gas::apply_gas_limit(estimated_or_fixed, gas_multiplier, chain_id, 21_000)
    }
}

async fn fetch_and_parse_gql(
    reporter: &dyn MintReporter,
    session: &opensea::AuthSession,
    slug: &str,
    addr: &alloy_primitives::Address,
    nft_contract: &str,
    chain: &str,
    stage_token_id: &str,
    quantity: u32,
    payment_asset: &serde_json::Value,
    calldata_value: &U256,
    mint_started_at: &std::time::Instant,
    attempt: u32,
    quiet: bool,
) -> anyhow::Result<(alloy_primitives::Address, U256, Bytes)> {
    let gql_start = std::time::Instant::now();
    let resp = opensea::fetch_mint_calldata(
        session,
        slug,
        addr,
        nft_contract,
        chain,
        stage_token_id,
        quantity,
        payment_asset,
    )
    .await?;

    let gql_ms = gql_start.elapsed().as_millis();
    if std::env::var("DEBUG").ok().as_deref() == Some("1") {
        let debug_file = format!("debug_gql_{}_{}.json", sign::shorten_address(addr), attempt);
        let _ = std::fs::write(
            &debug_file,
            serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string()),
        );
        mint_log(reporter, quiet,
            format!(
                "[{}] GQL fetch OK {}ms (saved {})",
                sign::shorten_address(addr),
                gql_ms,
                debug_file
            ),
        );
    } else {
        mint_log(reporter, quiet,
            format!(
                "[{}] GQL fetch OK {}ms",
                sign::shorten_address(addr),
                gql_ms,
            ),
        );
    }

    let tx_data = opensea::extract_opensea_action_tx(&resp)?;
    let to_addr: alloy_primitives::Address = tx_data
        .get("to")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_SEADROP_ADDRESS)
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid tx to"))?;
    let tx_value = tx_data
        .get("value")
        .and_then(|v| v.as_str())
        .and_then(parse_hex_u256)
        .unwrap_or(*calldata_value);
    if tx_value != *calldata_value {
        mint_log(reporter, quiet,
            format!(
                "[{}] WARN OpenSea tx value differs from selected phase price: gql_value={} parsed_phase_value={}",
                sign::shorten_address(addr),
                tx_value,
                calldata_value
            ),
        );
    }
    let data_hex = tx_data.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let cd = parse_tx_calldata_hex(data_hex).with_context(|| {
        format!(
            "[{}] OpenSea GQL tx data",
            sign::shorten_address(addr)
        )
    })?;

    mint_log(reporter, quiet,
        format!(
            "[{}] PREPARED t+{}ms (gql={}ms) to={:?} value={} data={} bytes",
            sign::shorten_address(addr),
            mint_started_at.elapsed().as_millis(),
            gql_ms,
            to_addr,
            tx_value,
            cd.len()
        ),
    );
    Ok((to_addr, tx_value, cd))
}

/// GQL fetch with one SIWE re-auth retry on 401 / auth errors.
async fn fetch_calldata_reauth(
    reporter: &dyn MintReporter,
    session: &mut Option<opensea::AuthSession>,
    signer: &Signer,
    chain_id: u64,
    proxy_url: Option<&str>,
    slug: &str,
    addr: &alloy_primitives::Address,
    nft_contract: &str,
    chain: &str,
    stage_token_id: &str,
    quantity: u32,
    payment_asset: &serde_json::Value,
    calldata_value: &U256,
    mint_started_at: &std::time::Instant,
    attempt: u32,
    quiet: bool,
) -> anyhow::Result<(alloy_primitives::Address, U256, Bytes)> {
    let sess = session
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no auth session"))?;
    match fetch_and_parse_gql(
        reporter,
        sess,
        slug,
        addr,
        nft_contract,
        chain,
        stage_token_id,
        quantity,
        payment_asset,
        calldata_value,
        mint_started_at,
        attempt,
        quiet,
    )
    .await
    {
        Ok(r) => Ok(r),
        Err(e) => {
            let err_str = format!("{}", e);
            if !is_auth_error(&err_str) {
                return Err(e);
            }
            mint_log(reporter, quiet,
                format!(
                    "[{}] {}: {}",
                    sign::shorten_address(addr),
                    crate::mint_ops::reauth_required_message(),
                    err_str
                ),
            );
            report_wallet(
                reporter,
                addr,
                Some(WalletStatus::Auth),
                Some(crate::mint_ops::reauth_required_message().into()),
                None,
                None,
            );
            let new_sess = opensea::siwe_auth(addr, signer, chain_id, proxy_url)
                .await
                .map_err(|ae| anyhow::anyhow!("re-auth failed: {}", ae))?;
            *session = Some(new_sess);
            let sess = session.as_ref().unwrap();
            fetch_and_parse_gql(
                reporter,
                sess,
                slug,
                addr,
                nft_contract,
                chain,
                stage_token_id,
                quantity,
                payment_asset,
                calldata_value,
                mint_started_at,
                attempt,
                quiet,
            )
            .await
        }
    }
}

fn cancelled(cancel: &Option<Arc<AtomicBool>>) -> bool {
    cancel
        .as_ref()
        .map(|c| c.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// OpenSea mint orchestration.
///
/// `cancel`: when set to true (best-effort), countdown aborts and workers stop
/// between attempts (in-flight RPC may still finish).
///
pub async fn run_opensea_mint(
    signers: &[Signer],
    env: &HashMap<String, String>,
    proxies: &ProxyManager,
    opts: &MintOptions,
    vault_password: Option<&str>,
    reporter: Arc<dyn MintReporter>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<MintRunSummary> {
    if signers.is_empty() {
        bail!("No wallets loaded. Add keys first.");
    }

    let slug = parse_collection_slug(&opts.slug);
    if slug.is_empty() {
        bail!("No collection specified");
    }

    // Full verbose trail on disk + UI reporter
    let (reporter, mint_log_path): (Arc<dyn MintReporter>, Option<String>) =
        match FileTeeReporter::create(reporter.clone(), &slug) {
            Ok(tee) => {
                let p = tee.path.display().to_string();
                log_always(&tee, format!("Full log file: {p}"));
                (Arc::new(tee) as Arc<dyn MintReporter>, Some(p))
            }
            Err(e) => {
                log_always(
                    reporter.as_ref(),
                    format!("WARN: could not open mint log file: {e}"),
                );
                (reporter, None)
            }
        };
    let _mint_log_path = mint_log_path;

    // Optional wallet subset: keep original vault indices for proxy mapping.
    // proxy_overrides: address → proxy list index (manual wallet→proxy map).
    let override_by_vault: std::collections::HashMap<usize, usize> = {
        let mut m = std::collections::HashMap::new();
        if let Some(ref ov) = opts.proxy_overrides {
            let by_addr: std::collections::HashMap<String, usize> = ov
                .iter()
                .map(|(a, idx)| (crate::api::normalize_address(a), *idx as usize))
                .collect();
            for (i, s) in signers.iter().enumerate() {
                let a = crate::api::normalize_address(&format!("{:?}", s.address()));
                if let Some(&pidx) = by_addr.get(&a) {
                    m.insert(i, pidx);
                }
            }
        }
        m
    };
    let (signers_owned, proxies_owned): (Vec<Signer>, ProxyManager) =
        if let Some(ref addrs) = opts.wallet_addresses {
            if addrs.is_empty() {
                bail!("No wallets selected for this task");
            }
            let want: std::collections::HashSet<String> = addrs
                .iter()
                .map(|a| crate::api::normalize_address(a))
                .collect();
            let mut selected = Vec::new();
            let mut orig_idx = Vec::new();
            for (i, s) in signers.iter().enumerate() {
                let a = crate::api::normalize_address(&format!("{:?}", s.address()));
                if want.contains(&a) {
                    selected.push(s.clone());
                    orig_idx.push(i);
                }
            }
            if selected.is_empty() {
                bail!("Selected wallets not found in unlocked vault");
            }
            log_always(
                reporter.as_ref(),
                format!(
                    "Task wallets: {}/{} selected",
                    selected.len(),
                    signers.len()
                ),
            );
            (
                selected,
                proxies.remap_for_indices_with_overrides(&orig_idx, &override_by_vault),
            )
        } else if override_by_vault.is_empty() {
            (signers.to_vec(), proxies.clone())
        } else {
            let all_idx: Vec<usize> = (0..signers.len()).collect();
            (
                signers.to_vec(),
                proxies.remap_for_indices_with_overrides(&all_idx, &override_by_vault),
            )
        };
    let signers: &[Signer] = &signers_owned;
    let proxies: &ProxyManager = &proxies_owned;

    let mut quantity = opts.quantity.max(1);
    let dry_run = opts.dry_run;
    let at_time = opts.at_time.clone();
    let auto_mode = true; // GUI/core always non-interactive phase pick

    let primary_addr = signers[0].address();
    let dummy_session = opensea::unauthenticated_session(&primary_addr);

    report_phase(
        reporter.as_ref(),
        "prep",
        format!("Preparing mint for «{slug}»…"),
    );
    log_always(reporter.as_ref(), format!("Fetching collection info for '{}'...", slug));
    let info = match opensea::collection_drop_info(&dummy_session, &slug, &primary_addr).await {
        Ok(i) => i,
        Err(_) => {
            log_always(reporter.as_ref(), "Collection info requires auth. Authenticating primary wallet...");
            let mut any_urls = collect_rpc_urls_for_chain(env, Some("ethereum"), &[]);
            if any_urls.is_empty() {
                any_urls = collect_rpc_urls_for_chain(env, None, &[]);
            }
            if any_urls.is_empty() {
                bail!("No RPC URLs for auth. Configure Alchemy or RPC in Settings.");
            }
            let mut any_rpc = rpc::RpcClient::new(any_urls);
            if let Err(e) = any_rpc.sort_by_fastest_provider().await {
                log_always(reporter.as_ref(), format!("RPC probe failed: {}", e));
            }
            let any_chain_id = any_rpc.chain_id().await.unwrap_or(1);
            match opensea::siwe_auth(&primary_addr, &signers[0], any_chain_id, None).await {
                Ok(session) => match opensea::collection_drop_info(&session, &slug, &primary_addr).await {
                    Ok(i) => i,
                    Err(e) => bail!("Failed collection info: {}", e),
                },
                Err(e) => bail!("Auth failed: {}", e),
            }
        }
    };

    let chain_for_rpc = opts
        .chain_override
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or(info.chain.as_str());
    if chain_for_rpc != info.chain {
        log_always(
            reporter.as_ref(),
            format!(
                "Chain override: RPC uses '{}' (collection reports '{}')",
                chain_for_rpc, info.chain
            ),
        );
    }

    let urls = collect_rpc_urls_for_chain(env, Some(chain_for_rpc), &[]);
    if urls.is_empty() {
        bail!(
            "No RPC URLs for chain '{}'. Set RPC in Settings.",
            chain_for_rpc
        );
    }
    log_always(
        reporter.as_ref(),
        format!("Using {} RPC URL(s) for {}", urls.len(), chain_for_rpc),
    );
    let mut rpc = rpc::RpcClient::new(urls.clone());
    if let Err(e) = rpc.sort_by_fastest_provider().await {
        log_always(reporter.as_ref(), format!("RPC probe failed: {}", e));
    }

    let actual_chain_id = rpc.chain_id().await.context("Failed to get chain ID from RPC")?;

    let use_flashbots = opts.use_flashbots.unwrap_or(false);
    if use_flashbots && actual_chain_id != MAINNET_CHAIN_ID {
        bail!(
            "Flashbots bundle only on Ethereum mainnet (chainId 1); RPC chainId is {}",
            actual_chain_id
        );
    }
    if use_flashbots {
        log_always(
            reporter.as_ref(),
            "Broadcast mode: Flashbots bundle (private, multi-wallet)".to_string(),
        );
    }

    let chain_map = chain_id_map();
    let expected_chain_id = chain_map
        .get(chain_for_rpc.to_lowercase().as_str())
        .copied()
        .or_else(|| chain_map.get(info.chain.to_lowercase().as_str()).copied());
    if let Some(expected) = expected_chain_id {
        if actual_chain_id != expected {
            bail!(
                "Chain mismatch: expected {} (chainId {}), but RPC returned chainId {}. Fix RPC or chain override.",
                chain_for_rpc, expected, actual_chain_id
            );
        }
    }

    // Limit parallel SIWE calls — OpenSea returns 429 when all wallets auth at once.
    // Direct IP: default 2. With proxies: up to unique proxy count (capped).
    let auth_concurrency: usize = env
        .get("AUTH_CONCURRENCY")
        .and_then(|v| v.trim().parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| crate::safety_policy::default_auth_concurrency(proxies.len()));
    report_phase(
        reporter.as_ref(),
        "auth",
        format!(
            "Authenticating {} wallet(s) on OpenSea (concurrency={})…",
            signers.len(),
            auth_concurrency
        ),
    );
    log_always(
        reporter.as_ref(),
        format!(
            "Authenticating all wallets (chainId={}, concurrency={})...",
            actual_chain_id, auth_concurrency
        ),
    );
    if crate::safety_policy::should_warn_no_proxy(signers.len(), proxies.len()) {
        log_always(
            reporter.as_ref(),
            crate::safety_policy::no_proxy_multi_wallet_message(signers.len()),
        );
    } else if proxies.is_empty() && signers.len() > 1 {
        log_always(
            reporter.as_ref(),
            "Tip: without proxies OpenSea rate-limits direct IP. Add proxies or AUTH_CONCURRENCY=1",
        );
    }

    let mut auth_cache = auth_cache::AuthCache::load(vault_password);
    let mut wallets: Vec<WalletAuth> = Vec::new();
    let mut auth_handles = Vec::new();
    let auth_sem = Arc::new(tokio::sync::Semaphore::new(auth_concurrency));
    // After first 429: serialize remaining auth (don't keep N-way hammering one IP).
    let force_serial = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let serial_mutex = Arc::new(tokio::sync::Mutex::new(()));

    for (i, signer) in signers.iter().enumerate() {
        let addr = signer.address();
        let addr_str = format!("{:?}", addr);
        let signer = signer.clone();
        let chain_id = actual_chain_id;
        let proxy_url = proxies.get(i).map(|s| s.to_string());
        let proxy_short = proxies.short(i);

        if let Some(cached_token) = auth_cache.get(&addr_str, chain_id) {
            let cookie_jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
            match opensea::build_client_with_cookie_jar_and_proxy(
                cookie_jar.clone(),
                proxy_url.as_deref(),
            ) {
                Ok(client) => {
                    log_always(reporter.as_ref(), format!("[{}] {:?} ... CACHED OK", i + 1, addr));
                    let session = opensea::AuthSession {
                        access_token: cached_token.to_string(),
                        address: addr_str,
                        client,
                        cookie_jar,
                    };
                    wallets.push(WalletAuth {
                        address: addr,
                        signer,
                        session: Some(session),
                        auth_ok: true,
                        nonce: 0,
                        prefetched_tx: None,
                        proxy_url,
                    });
                    continue;
                }
                Err(e) => {
                    log_always(reporter.as_ref(), format!("[{}] {:?} ... CACHED token but client build failed: {} — re-auth",
                        i + 1,
                        addr,
                        e));
                }
            }
        }

        let sem = auth_sem.clone();
        let force_serial = force_serial.clone();
        let serial_mutex = serial_mutex.clone();
        let stagger_ms = (i as u64 % auth_concurrency as u64) * 200;
        let rep = reporter.clone();
        let proxy_url_task = proxy_url.clone();
        auth_handles.push(tokio::spawn(async move {
            let proxy_url = proxy_url_task;
            let _permit = match sem.acquire().await {
                Ok(p) => p,
                Err(_) => {
                    return (addr, signer, None, false, None, proxy_url);
                }
            };
            // After any 429, remaining auths go one-at-a-time.
            let _serial_guard = if force_serial.load(std::sync::atomic::Ordering::SeqCst) {
                Some(serial_mutex.lock().await)
            } else {
                None
            };
            // Stagger start within the concurrency window.
            if stagger_ms > 0 && !force_serial.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(stagger_ms)).await;
            }

            let start = std::time::Instant::now();
            let result = opensea::siwe_auth(&addr, &signer, chain_id, proxy_url.as_deref()).await;
            let elapsed = start.elapsed().as_millis();
            match result {
                Ok(session) => {
                    log_always(
                        rep.as_ref(),
                        format!("[{}] {:?} OK ({}ms) via {}", i + 1, addr, elapsed, proxy_short),
                    );
                    (addr, signer, Some(session), true, None, proxy_url)
                }
                Err(e) => {
                    let err_s = format!("{e}");
                    if crate::safety_policy::is_rate_limit_error(&err_s) {
                        force_serial.store(true, std::sync::atomic::Ordering::SeqCst);
                        log_always(
                            rep.as_ref(),
                            format!(
                                "[{}] rate limit — {}",
                                sign::shorten_address(&addr),
                                crate::safety_policy::rate_limit_actionable_message()
                            ),
                        );
                    }
                    let msg = format!(
                        "[{}] {:?} FAILED ({}ms) via {}: {}",
                        i + 1,
                        addr,
                        elapsed,
                        proxy_short,
                        e
                    );
                    log_always(rep.as_ref(), msg.clone());
                    (addr, signer, None, false, Some(msg), proxy_url)
                }
            }
        }));
    }

    for handle in auth_handles {
        match handle.await {
            Ok((addr, signer, session, auth_ok, _err, proxy_url)) => {
                if auth_ok {
                    if let Some(ref sess) = session {
                        let addr_str = format!("{:?}", addr);
                        auth_cache.save(&addr_str, actual_chain_id, &sess.access_token);
                    }
                }
                wallets.push(WalletAuth {
                    address: addr,
                    signer,
                    session,
                    auth_ok,
                    nonce: 0,
                    prefetched_tx: None,
                    proxy_url,
                });
            }
            Err(e) => log_always(reporter.as_ref(), format!("Auth task failed: {}", e)),
        }
    }

    // One disk encrypt (PBKDF2) for the whole auth batch — not per wallet.
    if let Err(e) = auth_cache.flush() {
        log_always(
            reporter.as_ref(),
            format!("WARN: auth cache flush failed: {e}"),
        );
    }

    let auth_ok_count = wallets.iter().filter(|w| w.auth_ok).count();
    if auth_ok_count == 0 {
        bail!("All wallets failed authentication");
    }
    log_always(reporter.as_ref(), format!("Auth: {}/{} wallets OK", auth_ok_count, wallets.len()));

    log_always(reporter.as_ref(), format!("\nRe-fetching collection info with auth for eligibility..."));
    let primary = wallets.iter().find(|w| w.auth_ok).unwrap();
    let primary_session = primary.session.as_ref().unwrap();
    let info = match opensea::collection_drop_info(primary_session, &slug, &primary.address).await {
        Ok(i) => i,
        Err(e) => {
            log_always(reporter.as_ref(), format!("Failed to re-fetch collection info: {}", e));
            log_always(reporter.as_ref(), format!("Using unauthenticated data..."));
            info
        }
    };

    log_always(reporter.as_ref(), format!("\nCollection: {} ({})", info.name, info.slug));
    log_always(reporter.as_ref(), format!("Chain: {} (chainId {})", info.chain, actual_chain_id));
    if let Some(ref dt) = info.drop_type {
        log_always(reporter.as_ref(), format!("Drop type: {}", dt));
    }
    if !info.contracts.is_empty() {
        log_always(reporter.as_ref(), format!("NFT contract: {}", info.contracts[0]));
    }

    let stages = &info.stages;
    if stages.is_empty() {
        bail!("No drop stages found");
    }

    log_always(reporter.as_ref(), format!("\nPhases:"));
    let mut phase_labels = Vec::new();
    for (i, stage) in stages.iter().enumerate() {
        let price = stage
            .price_eth
            .map(|p| format!("{} ETH", p))
            .unwrap_or_else(|| "-".to_string());
        let eligible = opensea::stage_eligibility_label(stage);
        let label = opensea::stage_label(stage);
        let available = opensea::available_mint_quantity(&info, stage)
            .map(|q| format!("available={}", q))
            .unwrap_or_else(|| "available=?".to_string());
        let start = stage
            .start_time
            .map(|t| format!("start={:.0}", t))
            .unwrap_or_default();
        phase_labels.push(format!(
            "#{} {:30} {:12} {:10} {:12} {} {}",
            i + 1,
            label,
            stage.stage_type,
            eligible,
            available,
            price,
            start
        ));
        log_always(reporter.as_ref(), format!("  {} | {} | {} | {} | {} | {} | {}",
            i + 1,
            label,
            stage.stage_type,
            eligible,
            available,
            price,
            start));
    }
    if stages.iter().any(|stage| stage.stage_type == "SIGNED_PRESALE" && stage.is_eligible.is_none()) {
        log_always(reporter.as_ref(), format!("  Note: signed phase eligibility is unknown from OpenSea phase list; selected wallet availability is checked after phase selection."));
    }

    let default_pick = stages
        .iter()
        .enumerate()
        .filter(|(_, s)| opensea::stage_effective_eligible(s))
        .filter(|(_, s)| opensea::available_mint_quantity(&info, s).unwrap_or(0) > 0)
        .min_by_key(|(_, s)| {
            let is_public = s.stage_type == "PUBLIC_SALE";
            let has_started = s.start_time.map(|t| t as i64).unwrap_or(0) <= 0;
            (is_public as usize, !has_started as usize, s.stage_index.unwrap_or(0))
        })
        .map(|(i, _)| i)
        .unwrap_or_else(|| stages.iter().position(opensea::stage_effective_eligible).unwrap_or(0));
    if default_pick > 0 || stages.len() > 1 {
        let rec_stage = &stages[default_pick];
        let rec_available = opensea::available_mint_quantity(&info, rec_stage).unwrap_or(0);
        log_always(reporter.as_ref(), format!("  Recommended: #{} {} (available={}, fastest={})",
            default_pick + 1,
            opensea::stage_label(rec_stage),
            rec_available,
            if rec_stage.stage_type == "PUBLIC_SALE" { "local build" } else { "GQL" }));
    }
    let pick = if let Some(idx) = opts.phase_index {
        if idx >= stages.len() {
            bail!("phase_index {} out of range (0..{})", idx, stages.len());
        }
        idx
    } else {
        log_always(
            reporter.as_ref(),
            format!("Auto mode: using recommended phase #{}", default_pick + 1),
        );
        default_pick
    };
    let _ = (auto_mode, &phase_labels);
    let stage = &stages[pick];
    log_always(reporter.as_ref(), format!("Selected: {}", opensea::stage_label(stage)));
    if let Some(available) = opensea::available_mint_quantity(&info, stage) {
        if available == 0 {
            bail!("Selected phase has no NFTs available for this wallet");
        }
        if quantity > available {
            log_always(reporter.as_ref(), format!("Requested quantity {} exceeds available {}; using {}",
                quantity, available, available));
            quantity = available;
        } else {
            log_always(reporter.as_ref(), format!("Available for this wallet in selected phase: {}", available));
        }
        quantity = quantity.min(available).max(1);
    } else {
        log_always(reporter.as_ref(), format!("Available quantity for selected phase is unknown; using requested {}",
            quantity));
        quantity = quantity.max(1);
    }
    log_always(reporter.as_ref(), format!("Mint quantity: {}", quantity));

    let selected_stage_type = stage.stage_type.clone();
    let selected_stage_index = stage.stage_index;
    let mut wallet_quantities: std::collections::HashMap<alloy_primitives::Address, u32> =
        std::collections::HashMap::new();
    // Seed requested qty per wallet (task per-wallet map or default quantity).
    {
        let addrs: Vec<String> = wallets
            .iter()
            .filter(|w| w.auth_ok)
            .map(|w| format!("{:?}", w.address))
            .collect();
        let expanded = crate::mint_ops::expand_wallet_quantities(
            quantity,
            &addrs,
            opts.wallet_quantities.as_ref(),
        );
        for w in &wallets {
            if !w.auth_ok {
                continue;
            }
            let k = crate::mint_ops::normalize_addr_key(&format!("{:?}", w.address));
            let q = expanded.get(&k).copied().unwrap_or(quantity).max(1);
            wallet_quantities.insert(w.address, q);
        }
    }
    report_phase(
        reporter.as_ref(),
        "prep",
        format!(
            "Checking phase availability ({} wallets, parallel)…",
            wallets.iter().filter(|w| w.auth_ok).count()
        ),
    );
    log_always(
        reporter.as_ref(),
        format!("\nChecking selected phase availability per wallet (parallel)…"),
    );
    // Parallel OpenSea availability — bounded concurrency to limit 429s.
    {
        let avail_conc = env
            .get("AVAIL_CONCURRENCY")
            .and_then(|v| v.trim().parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or_else(|| {
                // Safer defaults: fewer parallel OS drop calls (less 429).
                if proxies.is_empty() {
                    2
                } else {
                    proxies.len().clamp(2, 4)
                }
            });
        let sem = Arc::new(tokio::sync::Semaphore::new(avail_conc));
        let mut handles = Vec::new();
        for w in &wallets {
            if !w.auth_ok {
                continue;
            }
            let Some(session) = w.session.clone() else {
                continue;
            };
            let addr = w.address;
            let requested = wallet_quantities.get(&addr).copied().unwrap_or(quantity);
            let slug = slug.clone();
            let selected_stage_type = selected_stage_type.clone();
            let selected_stage_index = selected_stage_index;
            let sem = sem.clone();
            let rep = reporter.clone();
            handles.push(tokio::spawn(async move {
                let _permit = match sem.acquire().await {
                    Ok(p) => p,
                    Err(_) => {
                        return (addr, requested, Ok::<u32, ()>(requested), None);
                    }
                };
                match opensea::collection_drop_info(&session, &slug, &addr).await {
                    Ok(wallet_info) => {
                        let wallet_stage = wallet_info.stages.iter().find(|s| {
                            s.stage_type == selected_stage_type
                                && s.stage_index == selected_stage_index
                        });
                        let available = wallet_stage
                            .and_then(|s| opensea::available_mint_quantity(&wallet_info, s))
                            .unwrap_or(requested);
                        let wallet_quantity = requested.min(available);
                        let msg = if wallet_quantity == 0 {
                            Some(format!(
                                "[{}] selected phase available=0, skipping wallet",
                                sign::shorten_address(&addr)
                            ))
                        } else if wallet_quantity < requested {
                            Some(format!(
                                "[{}] requested {} but available {}; using {}",
                                sign::shorten_address(&addr),
                                requested,
                                available,
                                wallet_quantity
                            ))
                        } else {
                            Some(format!(
                                "[{}] available={} quantity={}",
                                sign::shorten_address(&addr),
                                available,
                                wallet_quantity
                            ))
                        };
                        if let Some(ref m) = msg {
                            log_always(rep.as_ref(), m.clone());
                        }
                        (addr, requested, Ok(wallet_quantity), msg)
                    }
                    Err(e) => {
                        let m = format!(
                            "[{}] failed to check wallet availability: {}; using requested {}",
                            sign::shorten_address(&addr),
                            e,
                            requested
                        );
                        log_always(rep.as_ref(), m.clone());
                        (addr, requested, Ok(requested), Some(m))
                    }
                }
            }));
        }
        for h in handles {
            if let Ok((addr, _req, res, _msg)) = h.await {
                match res {
                    Ok(0) => {
                        if let Some(w) = wallets.iter_mut().find(|w| w.address == addr) {
                            w.auth_ok = false;
                        }
                        wallet_quantities.remove(&addr);
                    }
                    Ok(q) => {
                        wallet_quantities.insert(addr, q);
                    }
                    Err(_) => {}
                }
            }
        }
    }
    if wallets.iter().filter(|w| w.auth_ok).count() == 0 {
        bail!("No wallets have available mints in selected phase");
    }

    // Settings / .env → gas + retries + sniper flags.
    let mut gas_params = GasParams::from_env(env);
    let max_attempts = max_retries_from_env(env);
    let quiet = opts.quiet.unwrap_or_else(|| quiet_from_env(env));
    let skip_preflight_flag = opts
        .skip_preflight
        .unwrap_or_else(|| skip_preflight_from_env(env));
    let beep = beep_from_env(env);
    let do_export = export_results_from_env(env);
    let first_confirm = Arc::new(AtomicBool::new(false));
    let priority_input = opts
        .priority_fee_gwei
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !priority_input.is_empty() {
        match priority_input.parse::<f64>() {
            Ok(pg) if pg > 0.0 => {
                gas_params = gas_params.with_priority_gwei(pg);
            }
            _ => {
                log_always(reporter.as_ref(), format!("Invalid priority fee '{}', keeping env/settings gas params",
                    priority_input));
            }
        }
    }
    // Gas limit: MintOptions override → env/settings GAS_LIMIT. 0 = estimate (auto).
    let fixed_gas_limit: Option<u64> = {
        let gl = opts.gas_limit.unwrap_or_else(|| {
            env.get("GAS_LIMIT")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(250_000)
        });
        if gl == 0 {
            None
        } else {
            Some(gl)
        }
    };
    let skip_preflight = skip_preflight_flag && fixed_gas_limit.is_some();
    if skip_preflight_flag && fixed_gas_limit.is_none() {
        log_always(reporter.as_ref(), format!("WARN: SKIP_PREFLIGHT ignored because GAS_LIMIT=0 (estimate mode)"));
    }
    log_always(reporter.as_ref(), format!("Gas: mode={:?} base_mult={} gas_mult={} priority={} max_retries={} quiet={} skip_preflight={}",
        gas_params.mode,
        gas_params.base_fee_multiplier,
        gas_params.gas_multiplier,
        gas_params
            .priority_fee
            .map(|p| format!("{} gwei", p / U256::from(1_000_000_000u64)))
            .unwrap_or_else(|| "network".to_string()),
        max_attempts,
        quiet,
        skip_preflight));

    let nft_contract = info
        .contracts
        .first()
        .map(|c| c.as_str())
        .unwrap_or("0x0000000000000000000000000000000000000000");
    let payment_asset = opensea::stage_payment_asset(&info, stage);
    let price_wei = stage.price_wei.unwrap_or(U256::ZERO);
    let stage_type_owned = stage.stage_type.clone();
    let stage_token_id = opensea::stage_token_id(stage);
    let seadrop_address = env.get("SEADROP_ADDRESS").cloned();
    let fee_recipient = env.get("FEE_RECIPIENT").cloned();

    // Explicit schedule must parse cleanly — never silently fall back to phase start.
    let stage_start_ts: Option<i64> = if let Some(ref at_str) = at_time {
        match crate::mint_ops::parse_at_time_unix(at_str) {
            Ok(Some(ts)) => {
                log_always(
                    reporter.as_ref(),
                    format!("Scheduled mint at unix {ts} (from at_time={at_str})"),
                );
                Some(ts)
            }
            Ok(None) => {
                // Empty string after trim — treat as unset.
                stage.start_time.map(|t| t as i64)
            }
            Err(e) => {
                bail!("{e}. Use unix seconds/ms or ISO 8601 / RFC3339.");
            }
        }
    } else {
        stage.start_time.map(|t| t as i64)
    };

    let gas_info = {
        let prio = gas_params
            .priority_fee
            .map(|p| format!("{}gwei", p / U256::from(1_000_000_000u64)))
            .unwrap_or_else(|| "auto".to_string());
        let gl = fixed_gas_limit
            .map(|g| format!("fixed={}", g))
            .unwrap_or_else(|| "est".into());
        let mut s = format!(
            "{} prio={} base*{} gas*{} retries={}",
            gl, prio, gas_params.base_fee_multiplier, gas_params.gas_multiplier, max_attempts
        );
        if dry_run {
            s = format!("[DRY-RUN] {}", s);
        }
        s
    };
    for w in wallets.iter() {
        let px = w
            .proxy_url
            .as_deref()
            .map(crate::proxy::short_proxy)
            .unwrap_or_else(|| "direct".to_string());
        if w.auth_ok {
            report_wallet(
                reporter.as_ref(),
                &w.address,
                Some(WalletStatus::Wait),
                Some(format!("proxy={}", px)),
                None,
                None,
            );
        } else {
            report_wallet(
                reporter.as_ref(),
                &w.address,
                Some(WalletStatus::Failed),
                None,
                None,
                Some("auth failed".into()),
            );
        }
    }
    log_always(
        reporter.as_ref(),
        format!(
            "{} wallet(s) auth OK, qty={}, retries={}, gas={}",
            auth_ok_count, quantity, max_attempts, gas_info
        ),
    );

    let chain_id = actual_chain_id;

    log_always(reporter.as_ref(), format!("\nRefreshing nonces for all wallets..."));
    let mut nonce_handles = Vec::new();
    for w in &wallets {
        if !w.auth_ok {
            continue;
        }
        let rpc = rpc.clone();
        let addr = w.address;
        nonce_handles.push(tokio::spawn(async move {
            let result = rpc.nonce(&addr).await;
            (addr, result)
        }));
    }
    for handle in nonce_handles {
        if let Ok((addr, result)) = handle.await {
            if let Some(w) = wallets.iter_mut().find(|w| w.address == addr) {
                match result {
                    Ok(n) => {
                        w.nonce = n;
                        report_wallet(
                            reporter.as_ref(),
                            &w.address,
                            Some(WalletStatus::Wait),
                            Some(format!("nonce={}", n)),
                            None,
                            None,
                        );
                    }
                    Err(e) => {
                        w.auth_ok = false;
                        report_wallet(
                            reporter.as_ref(),
                            &w.address,
                            Some(WalletStatus::Failed),
                            None,
                            None,
                            Some(format!("nonce: {}", e)),
                        );
                    }
                }
            }
        }
    }

    log_always(reporter.as_ref(), format!("\nChecking balances..."));
    {
        let before_balance = wallets.iter().filter(|w| w.auth_ok).count();
        let (base_fee_check, priority_check) = rpc.fee_history().await
            .unwrap_or((U256::from(1_000_000_000u64), U256::from(1_000_000_000u64)));
        let estimated_gas_cost = U256::from(fixed_gas_limit.unwrap_or(250_000)) * (base_fee_check + priority_check);
        let mut bal_handles = Vec::new();
        for w in &wallets {
            if !w.auth_ok {
                continue;
            }
            let rpc = rpc.clone();
            let addr = w.address;
            let qty = wallet_quantities.get(&addr).copied().unwrap_or(quantity);
            let val = price_wei * U256::from(qty);
            bal_handles.push(tokio::spawn(async move {
                let bal = rpc.balance(&addr).await;
                (addr, bal, val)
            }));
        }
        for handle in bal_handles {
            if let Ok((addr, bal_result, mint_value)) = handle.await {
                if let Some(w) = wallets.iter_mut().find(|w| w.address == addr) {
                    match bal_result {
                        Ok(bal) => {
                            let needed = mint_value + estimated_gas_cost;
                            let bal_eth = format!("{:.6}", (bal / U256::from(1e12 as u64)).to::<u128>() as f64 / 1e6);
                            let need_eth = format!("{:.6}", (needed / U256::from(1e12 as u64)).to::<u128>() as f64 / 1e6);
                            if bal < needed {
                                let deficit = needed - bal;
                                let def_eth = format!("{:.6}", (deficit / U256::from(1e12 as u64)).to::<u128>() as f64 / 1e6);
                                log_always(reporter.as_ref(), format!("  [{}] LOW BALANCE: {} ETH (need {} ETH, deficit {} ETH)",
                                    sign::shorten_address(&addr),
                                    bal_eth,
                                    need_eth,
                                    def_eth));
                                // Both live and dry-run: do not count as OK without funds.
                                // Tx would not succeed (and estimateGas fails with OutOfFunds).
                                w.auth_ok = false;
                                report_wallet(
                                    reporter.as_ref(),
                                    &w.address,
                                    Some(WalletStatus::Failed),
                                    None,
                                    None,
                                    Some(format!(
                                        "insufficient balance: have {} ETH, need {} ETH",
                                        bal_eth, need_eth
                                    )),
                                );
                            } else {
                                log_always(reporter.as_ref(), format!("  [{}] balance={} ETH (need {} ETH) OK",
                                    sign::shorten_address(&addr),
                                    bal_eth,
                                    need_eth));
                            }
                        }
                        Err(e) => {
                            log_always(reporter.as_ref(), format!("  [{}] balance check failed: {}",
                                sign::shorten_address(&addr),
                                e));
                        }
                    }
                }
            }
        }
        let funded = wallets.iter().filter(|w| w.auth_ok).count();
        if funded == 0 {
            bail!("No wallets with sufficient balance to mint");
        }
        if funded < before_balance {
            log_always(reporter.as_ref(), format!("Balance gate: {}/{} wallets remaining after low-balance skip",
                funded, before_balance));
        }
    }

    // Proxy probe skipped on mint hot path (use Proxies page). Soft checklist only.
    let proxy_slots = if proxies.is_empty() {
        0
    } else {
        wallets.len().max(1)
    };
    log_always(
        reporter.as_ref(),
        format!(
            "\nProxies: {} configured — probe skipped on mint path (use Proxies page to health-check)",
            proxies.len()
        ),
    );

    let ready_wallets = wallets.iter().filter(|w| w.auth_ok).count();
    let mut checklist = vec![
        format!("✓ Collection   {}", info.slug),
        format!("✓ Phase        {}", opensea::stage_label(stage)),
        format!("✓ Chain        {} (id={})", info.chain, actual_chain_id),
        format!(
            "✓ Wallets      {} ready / {} total (qty={})",
            ready_wallets,
            wallets.len(),
            quantity
        ),
        format!(
            "✓ Proxies      {} listed (probe skipped on mint)",
            proxies.len()
        ),
        format!(
            "✓ Gas          {} · skip_preflight={} · quiet={}",
            gas_info, skip_preflight, quiet
        ),
        format!(
            "✓ Price        {} wei × qty",
            price_wei
        ),
    ];
    let _ = proxy_slots;
    if let Some(ts) = stage_start_ts {
        checklist.push(format!("✓ Start        unix {}", ts));
    }
    let mut can_start = ready_wallets > 0;
    if ready_wallets == 0 {
        checklist.push("! No wallets ready to mint".into());
        can_start = false;
    }

    log_always(reporter.as_ref(), "--- Checklist ---");
    for l in &checklist {
        log_always(reporter.as_ref(), l.clone());
    }
    if !can_start {
        bail!("Cannot start mint — no wallets ready");
    }

    if let Some(start_ts) = stage_start_ts {
        // OpenSea stage.start_time is wall-clock (unix). Waiting on eth block.timestamp
        // lags ~1 block (~12s on L1) — that is why logs showed "opens in ~1s" then
        // "Phase is open!" only ~12s later. Fire on wall clock.
        let open_at = chrono::DateTime::from_timestamp(start_ts, 0)
            .map(|d| d.format("%H:%M:%S UTC").to_string())
            .unwrap_or_else(|| start_ts.to_string());
        report_phase(
            reporter.as_ref(),
            "wait",
            format!("Waiting for phase open at {open_at}…"),
        );
        log_always(
            reporter.as_ref(),
            format!("\nWaiting for phase open (wall clock) at {open_at} (unix={start_ts})"),
        );
        let mut nonce_refreshed = false;
        let mut prefetched = false;
        let mut last_printed = -1i64;
        let mut logged_chain_lag = false;

        let use_gql = opts.use_gql.unwrap_or_else(|| {
            env.get("USE_GQL").map(|v| v == "1" || v == "true").unwrap_or(false)
        });
        let should_prefetch = stage_type_owned != "PUBLIC_SALE" || use_gql;

        let mut prefetch_handles: Vec<
            tokio::task::JoinHandle<(
                alloy_primitives::Address,
                Option<(alloy_primitives::Address, U256, Bytes)>,
            )>,
        > = Vec::new();

        loop {
            if cancelled(&cancel) {
                bail!("Mint cancelled while waiting for phase open");
            }
            let wall_ts = chrono::Utc::now().timestamp();
            let left = start_ts - wall_ts;
            if left <= 0 {
                break;
            }

            if left != last_printed {
                log_always(
                    reporter.as_ref(),
                    format!("{left}s until phase open (wall clock)"),
                );
                if left <= 30 || left % 30 == 0 {
                    let m = left / 60;
                    let s = left % 60;
                    report_phase(
                        reporter.as_ref(),
                        "wait",
                        if m > 0 {
                            format!("Phase opens in ~{m}m {s:02}s")
                        } else {
                            format!("Phase opens in ~{s}s")
                        },
                    );
                }
                last_printed = left;
            }

            // One-shot diagnostic: chain block.timestamp often lags wall by ~block time.
            if !logged_chain_lag && left <= 5 {
                logged_chain_lag = true;
                if let Ok(chain_ts) = rpc.block_timestamp().await {
                    let chain_left = start_ts - chain_ts as i64;
                    if chain_left > 0 {
                        log_always(
                            reporter.as_ref(),
                            format!(
                                "Note: chain block.timestamp still {chain_left}s behind open — \
                                 we fire on wall clock (not chain), so we do not wait for that lag"
                            ),
                        );
                    }
                }
            }

            if !prefetched && should_prefetch && left <= 5 {
                log_always(
                    reporter.as_ref(),
                    format!("\n  Pre-fetching calldata ({left}s before open)..."),
                );
                prefetched = true;
                for w in &wallets {
                    if !w.auth_ok || w.session.is_none() {
                        continue;
                    }
                    let session = w.session.clone().unwrap();
                    let pf_slug = slug.clone();
                    let addr = w.address;
                    let pf_nft_contract = nft_contract.to_string();
                    let pf_chain = info.chain.clone();
                    let pf_stage_token_id = stage_token_id.clone();
                    let pf_quantity = wallet_quantities.get(&addr).copied().unwrap_or(quantity);
                    let pf_payment_asset = payment_asset.clone();
                    let pf_calldata_value = price_wei * U256::from(pf_quantity);

                    let rep = reporter.clone();
                    prefetch_handles.push(tokio::spawn(async move {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(10);
                        loop {
                            if std::time::Instant::now() > deadline {
                                return (addr, None);
                            }
                            match fetch_and_parse_gql(
                                &NullReporter,
                                &session,
                                &pf_slug,
                                &addr,
                                &pf_nft_contract,
                                &pf_chain,
                                &pf_stage_token_id,
                                pf_quantity,
                                &pf_payment_asset,
                                &pf_calldata_value,
                                &std::time::Instant::now(),
                                0,
                                false,
                            )
                            .await
                            {
                                Ok(result) => {
                                    log_always(
                                        rep.as_ref(),
                                        format!(
                                            "[{}] pre-fetch OK",
                                            sign::shorten_address(&addr)
                                        ),
                                    );
                                    return (addr, Some(result));
                                }
                                Err(_) => {
                                    tokio::time::sleep(std::time::Duration::from_millis(500))
                                        .await;
                                }
                            }
                        }
                    }));
                }
            }

            if !nonce_refreshed && left <= 2 {
                log_always(
                    reporter.as_ref(),
                    "\n  Refreshing nonces (2s before open)...".to_string(),
                );
                let mut refresh_handles = Vec::new();
                for w in &wallets {
                    if !w.auth_ok {
                        continue;
                    }
                    let rpc = rpc.clone();
                    let addr = w.address;
                    refresh_handles.push(tokio::spawn(async move {
                        let result = rpc.nonce(&addr).await;
                        (addr, result)
                    }));
                }
                for handle in refresh_handles {
                    if let Ok((addr, result)) = handle.await {
                        if let Some(w) = wallets.iter_mut().find(|w| w.address == addr) {
                            if let Ok(n) = result {
                                w.nonce = n;
                            }
                        }
                    }
                }
                nonce_refreshed = true;
            }

            // Tighter poll near open so we don't overshoot by hundreds of ms.
            let sleep_ms = if left <= 2 { 25 } else if left <= 10 { 50 } else { 200 };
            tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
        }

        let fire_lag_ms = fire_lag_ms_from_clock(start_ts, chrono::Utc::now().timestamp_millis());
        log_always(
            reporter.as_ref(),
            format!(
                "Phase is open! (wall clock, fire lag ~{fire_lag_ms}ms after start_ts)"
            ),
        );

        // Collect prefetched calldata; don't block long if a wallet still retrying.
        let join_deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(800);
        for handle in prefetch_handles {
            let left_ms = join_deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u64;
            if left_ms == 0 {
                handle.abort();
                continue;
            }
            match tokio::time::timeout(
                std::time::Duration::from_millis(left_ms.max(1)),
                handle,
            )
            .await
            {
                Ok(Ok((addr, Some(tx_data)))) => {
                    if let Some(w) = wallets.iter_mut().find(|w| w.address == addr) {
                        w.prefetched_tx = Some(tx_data);
                    }
                }
                Ok(Ok((_, None))) => {}
                Ok(Err(_)) => {}
                Err(_elapsed) => {
                    // timeout — mint worker will fetch calldata on the fly
                }
            }
        }
    }
    report_phase(
        reporter.as_ref(),
        "fire",
        "Phase open — sending mints now…",
    );
    log_always(reporter.as_ref(), "Phase open — starting mint workers");

    let (base_fee, network_priority) = rpc
        .fee_history()
        .await
        .unwrap_or((U256::from(1_000_000_000u64), U256::from(1_000_000_000u64)));
    let (max_fee, max_priority_fee) =
        gas::calculate_fees(&gas_params, base_fee, network_priority).unwrap_or((
            base_fee * U256::from(2u64) + network_priority,
            network_priority,
        ));

    if cancelled(&cancel) {
        bail!("Mint cancelled before workers started");
    }

    let rpc_clone = rpc.clone();
    let mint_started_at = std::time::Instant::now();
    let fb_pieces: Arc<std::sync::Mutex<Vec<BundleTx>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let mut handles = tokio::task::JoinSet::new();

    for (_, w) in wallets.iter().enumerate() {
        if !w.auth_ok {
            continue;
        }

        let rpc = rpc_clone.clone();
        let signer = w.signer.clone();
        let mut session = w.session.clone();
        let addr = w.address;
        let auth_chain_id = chain_id;
        let mut nonce = w.nonce;
        let gas_multiplier = gas_params.gas_multiplier;
        let nft_contract_owned = nft_contract.to_string();
        let chain_owned = info.chain.clone();
        let slug_owned = slug.clone();
        let payment_asset_owned = payment_asset.clone();
        let quantity_owned = wallet_quantities.get(&addr).copied().unwrap_or(quantity);
        let calldata_value = price_wei * U256::from(quantity_owned);
        let stage_type_owned = stage_type_owned.clone();
        let stage_token_id_owned = stage_token_id.clone();
        let seadrop_address_owned = seadrop_address.clone();
        let fee_recipient_owned = fee_recipient.clone();
        let mint_started_at = mint_started_at;
        let fixed_gas_limit_owned = fixed_gas_limit;
        let max_fee = max_fee;
        let max_priority_fee = max_priority_fee;
        let use_gql_owned = opts.use_gql.unwrap_or_else(|| {
            env.get("USE_GQL").map(|v| v == "1" || v == "true").unwrap_or(false)
        });
        let initial_cached_tx = w.prefetched_tx.clone();
        let max_attempts = max_attempts;
        let reporter = reporter.clone();
        let quiet_w = quiet;
        let skip_preflight_w = skip_preflight;
        let skip_estimate_on_open = opts.skip_estimate_on_open.unwrap_or(false);
        let beep_w = beep;
        let first_confirm_w = first_confirm.clone();
        let cancel_w = cancel.clone();
        let use_flashbots_w = use_flashbots;
        let fb_pieces_w = fb_pieces.clone();
        let dry_run_w = dry_run;
        // Bound at auth time (signer index) — never re-derive from wallets order.
        let proxy_url_w = w.proxy_url.clone();

        handles.spawn(async move {
            let mut attempt = 0u32;
            let burst_delays = [0u64, 50, 100, 200, 500, 1000, 2000, 3000];
            let mut burst_idx = 0usize;
            // Prefetch / retry calldata. Use take()+restore so every write is later read.
            let mut cached_tx: Option<(alloy_primitives::Address, U256, Bytes)> = initial_cached_tx;
            // Last failure message for exhaust path only (updated in place via helper).
            let mut last_error = String::new();
            let mut max_fee = max_fee;
            let mut max_priority_fee = max_priority_fee;

            loop {
                if cancelled(&cancel_w) {
                    report_wallet(
                        reporter.as_ref(),
                        &addr,
                        Some(WalletStatus::Failed),
                        Some("cancelled".into()),
                        None,
                        Some("cancelled by user".into()),
                    );
                    break (
                        addr,
                        MintResult {
                            address: addr,
                            tx_hash: None,
                            status: WalletStatus::Failed,
                            gas_used: None,
                            block_number: None,
                            error: Some("cancelled by user".into()),
                        },
                    );
                }
                attempt += 1;
                if attempt > max_attempts {
                    let err = if last_error.is_empty() {
                        "mint retries exhausted".to_string()
                    } else {
                        std::mem::take(&mut last_error)
                    };
                    report_wallet(reporter.as_ref(),
                        &addr,
                        Some(WalletStatus::Failed),
                        Some(format!("attempt {}/{}", attempt - 1, max_attempts)),
                        None,
                        Some(err.clone()),
                    );
                    break (
                        addr,
                        MintResult {
                            address: addr,
                            tx_hash: None,
                            status: WalletStatus::Failed,
                            gas_used: None,
                            block_number: None,
                            error: Some(err),
                        },
                    );
                }

                report_wallet(reporter.as_ref(),
                    &addr,
                    Some(WalletStatus::Calldata),
                    Some(format!("attempt {}/{}", attempt, max_attempts)),
                    None,
                    None,
                );

                // `.take()`: consume cache for this attempt; put back only on fee/nonce retry.
                let (to_addr, tx_value, calldata): (alloy_primitives::Address, U256, Bytes) =
                    if let Some((to, val, cd)) = cached_tx.take() {
                        (to, val, cd)
                    } else if stage_type_owned == "PUBLIC_SALE" && !use_gql_owned {
                        let local_start = std::time::Instant::now();
                        let addr_str = format!("{:?}", addr);
                        let local_built: Result<(alloy_primitives::Address, U256, Bytes), String> =
                            match opensea::build_public_mint_tx(
                                &nft_contract_owned,
                                quantity_owned,
                                calldata_value / U256::from(quantity_owned.max(1)),
                                seadrop_address_owned.as_deref(),
                                fee_recipient_owned.as_deref(),
                                Some(&addr_str),
                            ) {
                                Ok(tx_data) => {
                                    let local_ms = local_start.elapsed().as_millis();
                                    let to_a: alloy_primitives::Address = tx_data
                                        .get("to")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(DEFAULT_SEADROP_ADDRESS)
                                        .parse()
                                        .unwrap_or_else(|_| {
                                            DEFAULT_SEADROP_ADDRESS.parse().unwrap_or_default()
                                        });
                                    let val = tx_data
                                        .get("value")
                                        .and_then(|v| v.as_str())
                                        .and_then(parse_hex_u256)
                                        .unwrap_or(calldata_value);
                                    let data_hex =
                                        tx_data.get("data").and_then(|v| v.as_str()).unwrap_or("");
                                    match parse_tx_calldata_hex(data_hex) {
                                        Ok(cd) => {
                                            log_always(
                                                reporter.as_ref(),
                                                format!(
                                                    "[{}] LOCAL BUILD OK {}ms to={:?} value={} data={} bytes",
                                                    sign::shorten_address(&addr),
                                                    local_ms,
                                                    to_a,
                                                    val,
                                                    cd.len()
                                                ),
                                            );
                                            Ok((to_a, val, cd))
                                        }
                                        Err(e) => Err(format!("invalid calldata: {e}")),
                                    }
                                }
                                Err(e) => Err(e.to_string()),
                            };
                        match local_built {
                            Ok(triple) => triple,
                            Err(e) => {
                                mint_log(
                                    reporter.as_ref(),
                                    quiet_w,
                                    format!(
                                        "[{}] LOCAL BUILD FAILED: {}, falling back to GQL",
                                        sign::shorten_address(&addr),
                                        e
                                    ),
                                );
                                if session.is_none() {
                                    break (
                                        addr,
                                        MintResult {
                                            address: addr,
                                            tx_hash: None,
                                            status: WalletStatus::Failed,
                                            gas_used: None,
                                            block_number: None,
                                            error: Some("no auth session".to_string()),
                                        },
                                    );
                                }
                                match fetch_calldata_reauth(
                                    reporter.as_ref(),
                                    &mut session,
                                    &signer,
                                    auth_chain_id,
                                    proxy_url_w.as_deref(),
                                    &slug_owned,
                                    &addr,
                                    &nft_contract_owned,
                                    &chain_owned,
                                    &stage_token_id_owned,
                                    quantity_owned,
                                    &payment_asset_owned,
                                    &calldata_value,
                                    &mint_started_at,
                                    attempt,
                                    quiet_w,
                                )
                                .await
                                {
                                    Ok(result) => result,
                                    Err(e) => {
                                        let err_str = format!("{}", e);
                                        if attempt > 3 {
                                            break (
                                                addr,
                                                MintResult {
                                                    address: addr,
                                                    tx_hash: None,
                                                    status: WalletStatus::Failed,
                                                    gas_used: None,
                                                    block_number: None,
                                                    error: Some(err_str),
                                                },
                                            );
                                        }
                                        last_error = err_str;
                                        tokio::time::sleep(std::time::Duration::from_millis(100))
                                            .await;
                                        continue;
                                    }
                                }
                            }
                        }
                    } else {
                        if session.is_none() {
                            break (
                                addr,
                                MintResult {
                                    address: addr,
                                    tx_hash: None,
                                    status: WalletStatus::Failed,
                                    gas_used: None,
                                    block_number: None,
                                    error: Some("no auth session".to_string()),
                                },
                            );
                        }
                        // UI surfaces re-auth on 401 via message (see mint_ops::reauth_required_message)
                match fetch_calldata_reauth(
                            reporter.as_ref(),
                            &mut session,
                            &signer,
                            auth_chain_id,
                            proxy_url_w.as_deref(),
                            &slug_owned,
                            &addr,
                            &nft_contract_owned,
                            &chain_owned,
                            &stage_token_id_owned,
                            quantity_owned,
                            &payment_asset_owned,
                            &calldata_value,
                            &mint_started_at,
                            attempt,
                            quiet_w,
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(e) => {
                                let err_str = format!("{}", e);
                                if attempt > 3 {
                                    break (
                                        addr,
                                        MintResult {
                                            address: addr,
                                            tx_hash: None,
                                            status: WalletStatus::Failed,
                                            gas_used: None,
                                            block_number: None,
                                            error: Some(err_str),
                                        },
                                    );
                                }
                                last_error = err_str;
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                continue;
                            }
                        }
                    };

                let gas_limit = if skip_preflight_w || skip_estimate_on_open {
                    let fixed_raw = fixed_gas_limit_owned.unwrap_or(250_000);
                    let fixed = resolve_mint_gas_limit(fixed_raw, gas_multiplier, chain_id, true);
                    let label = if skip_preflight_w {
                        "SKIP_PREFLIGHT"
                    } else {
                        "SKIP_ESTIMATE_ON_OPEN"
                    };
                    if fixed != fixed_raw {
                        mint_log(
                            reporter.as_ref(),
                            quiet_w,
                            format!(
                                "[{}] {} gas_limit {} → {} (L2 floor)",
                                sign::shorten_address(&addr),
                                label,
                                fixed_raw,
                                fixed
                            ),
                        );
                    }
                    report_wallet(reporter.as_ref(),
                        &addr,
                        Some(WalletStatus::Sim),
                        Some(format!("{label} gas={fixed}")),
                        None,
                        None,
                    );
                    mint_log(reporter.as_ref(), quiet_w,
                        format!(
                            "[{}] {} gas_limit={}",
                            sign::shorten_address(&addr),
                            label,
                            fixed
                        ),
                    );
                    fixed
                } else if let Some(fixed_raw) = fixed_gas_limit_owned {
                    // Manual gas: still require preflight OK before any send (Start → sim → tx).
                    let fixed = resolve_mint_gas_limit(fixed_raw, gas_multiplier, chain_id, true);
                    if fixed != fixed_raw {
                        mint_log(
                            reporter.as_ref(),
                            quiet_w,
                            format!(
                                "[{}] fixed gas {} → {} (L2 floor)",
                                sign::shorten_address(&addr),
                                fixed_raw,
                                fixed
                            ),
                        );
                    }
                    report_wallet(reporter.as_ref(),
                        &addr,
                        Some(WalletStatus::Sim),
                        Some(format!("sim attempt {}", attempt)),
                        None,
                        None,
                    );
                    let sim_start = std::time::Instant::now();
                    match rpc.estimate_gas(&addr, &to_addr, tx_value, &calldata).await {
                        Ok(g) => {
                            mint_log(reporter.as_ref(), quiet_w,
                                format!(
                                    "[{}] pre-flight OK {}ms est={} limit={}",
                                    sign::shorten_address(&addr),
                                    sim_start.elapsed().as_millis(),
                                    g,
                                    fixed
                                ),
                            );
                            report_wallet(reporter.as_ref(),
                                &addr,
                                Some(WalletStatus::Sim),
                                Some(format!("preflight ok est={} limit={}", g, fixed)),
                                None,
                                None,
                            );
                        }
                        Err(e) => {
                            let err_str = format!("{}", e);
                            mint_log(reporter.as_ref(), quiet_w,
                                format!(
                                    "[{}] PRE-FLIGHT FAIL {}ms: {}",
                                    sign::shorten_address(&addr),
                                    sim_start.elapsed().as_millis(),
                                    err_str
                                ),
                            );
                            // Never send after failed sim — fatal fail, retryable retry.
                            match classify_mint_error(&err_str) {
                                "fatal" => {
                                    report_wallet(reporter.as_ref(),
                                        &addr,
                                        Some(WalletStatus::Failed),
                                        None,
                                        None,
                                        Some(err_str.clone()),
                                    );
                                    break (
                                        addr,
                                        MintResult {
                                            address: addr,
                                            tx_hash: None,
                                            status: WalletStatus::Failed,
                                            gas_used: None,
                                            block_number: None,
                                            error: Some(err_str),
                                        },
                                    );
                                }
                                _ => {
                                    if attempt == 1 || attempt % 5 == 0 {
                                        log_always(reporter.as_ref(), format!(
                                            "[{}] pre-flight retry {}/{}: {}",
                                            sign::shorten_address(&addr),
                                            attempt,
                                            max_attempts,
                                            err_str
                                        ));
                                    }
                                    last_error = err_str;
                                    let delay_ms = if burst_idx < burst_delays.len() {
                                        burst_delays[burst_idx]
                                    } else {
                                        100
                                    };
                                    burst_idx += 1;
                                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                                        .await;
                                    continue;
                                }
                            }
                        }
                    }
                    fixed
                } else {
                    let est_gas_start = std::time::Instant::now();
                    let gas_estimate =
                        match rpc.estimate_gas(&addr, &to_addr, tx_value, &calldata).await {
                            Ok(g) => {
                                log_always(reporter.as_ref(), format!("[{}] estimate_gas OK {}ms gas={}",
                                    sign::shorten_address(&addr),
                                    est_gas_start.elapsed().as_millis(),
                                    g));
                                report_wallet(reporter.as_ref(),
                                    &addr,
                                    Some(WalletStatus::Sim),
                                    Some(format!("est gas={}", g)),
                                    None,
                                    None,
                                );
                                g
                            }
                            Err(e) => {
                                log_always(reporter.as_ref(), format!("[{}] estimate_gas FAIL {}ms: {}",
                                    sign::shorten_address(&addr),
                                    est_gas_start.elapsed().as_millis(),
                                    e));
                                let err_str = format!("{}", e);
                                match classify_mint_error(&err_str) {
                                    "fatal" => {
                                        report_wallet(reporter.as_ref(),
                                            &addr,
                                            Some(WalletStatus::Failed),
                                            None,
                                            None,
                                            Some(err_str.clone()),
                                        );
                                        break (
                                            addr,
                                            MintResult {
                                                address: addr,
                                                tx_hash: None,
                                                status: WalletStatus::Failed,
                                                gas_used: None,
                                                block_number: None,
                                                error: Some(err_str),
                                            },
                                        );
                                    }
                                    _ => {
                                        if attempt == 1 || attempt % 5 == 0 {
                                            log_always(reporter.as_ref(), format!("[{}] estimate_gas retry {}/{}: {}",
                                                sign::shorten_address(&addr),
                                                attempt,
                                                max_attempts,
                                                err_str));
                                        }
                                        last_error = err_str;
                                        // leave cached_tx empty (already taken) → rebuild next attempt
                                        let delay_ms = if burst_idx < burst_delays.len() {
                                            burst_delays[burst_idx]
                                        } else {
                                            100
                                        };
                                        burst_idx += 1;
                                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                                            .await;
                                        continue;
                                    }
                                }
                            }
                        };
                    let limit =
                        resolve_mint_gas_limit(gas_estimate, gas_multiplier, chain_id, false);
                    mint_log(
                        reporter.as_ref(),
                        quiet_w,
                        format!(
                            "[{}] gas_limit={} (est={} mult={})",
                            sign::shorten_address(&addr),
                            limit,
                            gas_estimate,
                            gas_multiplier
                        ),
                    );
                    limit
                };

                // Public dry-run stops before sign. Flashbots dry-run signs for eth_callBundle.
                if dry_run_w && !use_flashbots_w {
                    let gas_cost_eth = (gas_limit as f64 * max_fee.to::<u128>() as f64) / 1e18;
                    let price_eth = (calldata_value.to::<u128>() as f64) / 1e18;
                    let total_eth = gas_cost_eth + price_eth;
                    log_always(reporter.as_ref(), format!("\n[{}] DRY RUN REPORT t+{}ms",
                        sign::shorten_address(&addr),
                        mint_started_at.elapsed().as_millis(),));
                    log_always(reporter.as_ref(), format!("  Contract:    {:?}", to_addr));
                    log_always(reporter.as_ref(), format!("  Value:       {} ETH ({} wei)", price_eth, tx_value));
                    log_always(reporter.as_ref(), format!("  Gas limit:   {}", gas_limit));
                    log_always(reporter.as_ref(), format!("  Max fee:     {} gwei", max_fee / U256::from(1_000_000_000u64)));
                    log_always(reporter.as_ref(), format!("  Priority:    {} gwei", max_priority_fee / U256::from(1_000_000_000u64)));
                    log_always(reporter.as_ref(), format!("  Gas cost:    ~{:.6} ETH", gas_cost_eth));
                    log_always(reporter.as_ref(), format!("  Total cost:  ~{:.6} ETH", total_eth));
                    log_always(reporter.as_ref(), format!("  Calldata:    {} bytes", calldata.len()));
                    log_always(reporter.as_ref(), format!("  Nonce:       {}", nonce));
                    log_always(reporter.as_ref(), format!("  Chain:       {} (id={})", chain_id, chain_id));
                    // Dry-run OK only if simulation path did not leave a preflight error.
                    // (Funds/sim failures already break Failed above when fixed gas + estimate.)
                    report_wallet(reporter.as_ref(),
                        &addr,
                        Some(WalletStatus::DryRunOk),
                        Some(format!("~{:.4} ETH total (sim OK, no broadcast)", total_eth)),
                        None,
                        None,
                    );
                    break (
                        addr,
                        MintResult {
                            address: addr,
                            tx_hash: None,
                            status: WalletStatus::DryRunOk,
                            gas_used: Some(gas_limit),
                            block_number: None,
                            error: None,
                        },
                    );
                }

                let sign_start = std::time::Instant::now();
                let tx = sign::BuiltTx {
                    chain_id,
                    nonce,
                    to: to_addr,
                    value: tx_value,
                    data: calldata,
                    gas_limit,
                    max_fee,
                    max_priority_fee,
                };
                let (raw, signed_hash) = match sign::sign_transaction(&signer, &tx) {
                    Ok((r, h)) => {
                        log_always(reporter.as_ref(), format!("[{}] sign OK {}ms raw={} bytes",
                            sign::shorten_address(&addr),
                            sign_start.elapsed().as_millis(),
                            r.len()));
                        (r, h)
                    }
                    Err(e) => {
                        break (
                            addr,
                            MintResult {
                                address: addr,
                                tx_hash: None,
                                status: WalletStatus::Failed,
                                gas_used: None,
                                block_number: None,
                                error: Some(format!("sign: {}", e)),
                            },
                        );
                    }
                };

                // Flashbots: collect signed txs; coordinator submits one bundle.
                if use_flashbots_w {
                    if let Ok(mut g) = fb_pieces_w.lock() {
                        g.push(BundleTx {
                            from: addr,
                            raw: raw.clone(),
                            tx_hash: signed_hash,
                        });
                    }
                    if dry_run_w {
                        report_wallet(
                            reporter.as_ref(),
                            &addr,
                            Some(WalletStatus::DryRunOk),
                            Some("signed for callBundle".into()),
                            Some(signed_hash),
                            None,
                        );
                        break (
                            addr,
                            MintResult {
                                address: addr,
                                tx_hash: Some(signed_hash),
                                status: WalletStatus::DryRunOk,
                                gas_used: Some(gas_limit),
                                block_number: None,
                                error: Some("__flashbots_dry__".into()),
                            },
                        );
                    }
                    report_wallet(
                        reporter.as_ref(),
                        &addr,
                        Some(WalletStatus::Sent),
                        Some("queued for Flashbots bundle".into()),
                        Some(signed_hash),
                        None,
                    );
                    break (
                        addr,
                        MintResult {
                            address: addr,
                            tx_hash: Some(signed_hash),
                            status: WalletStatus::Sent,
                            gas_used: Some(gas_limit),
                            block_number: None,
                            error: Some("__flashbots_pending__".into()),
                        },
                    );
                }

                let send_start = std::time::Instant::now();
                report_wallet(reporter.as_ref(),
                    &addr,
                    Some(WalletStatus::Sent),
                    Some("broadcasting...".into()),
                    None,
                    None,
                );
                let tx_hash = match rpc.race_send(&raw).await {
                    Ok(h) => {
                        log_always(reporter.as_ref(), format!("[{}] SEND OK {}ms tx={}",
                            sign::shorten_address(&addr),
                            send_start.elapsed().as_millis(),
                            sign::shorten_hash(&h)));
                        // Always wait for on-chain receipt — SENT alone is not success.
                        report_wallet(reporter.as_ref(),
                            &addr,
                            Some(WalletStatus::Sent),
                            Some("pending receipt".into()),
                            Some(h),
                            None,
                        );
                        h
                    }
                    Err(e) => {
                        log_always(reporter.as_ref(), format!("[{}] SEND FAIL {}ms: {}",
                            sign::shorten_address(&addr),
                            send_start.elapsed().as_millis(),
                            e));
                        let err_str = format!("{}", e);
                        if is_already_known(&err_str) {
                            report_wallet(reporter.as_ref(),
                                &addr,
                                Some(WalletStatus::Sent),
                                Some("already known".into()),
                                Some(signed_hash),
                                None,
                            );
                            signed_hash
                        } else if is_nonce_too_low(&err_str) {
                            // Same calldata next attempt; only nonce changes.
                            cached_tx = Some((tx.to, tx.value, tx.data.clone()));
                            if let Ok(n) = rpc.nonce(&addr).await {
                                nonce = n;
                            }
                            let delay_ms = if burst_idx < burst_delays.len() {
                                burst_delays[burst_idx]
                            } else {
                                100
                            };
                            burst_idx += 1;
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            continue;
                        } else if is_underpriced(&err_str) {
                            // Same calldata next attempt; only gas bumps (×1.15).
                            cached_tx = Some((tx.to, tx.value, tx.data.clone()));
                            max_fee = gas::bump_fee_bps(max_fee, 11_500);
                            max_priority_fee = gas::bump_fee_bps(max_priority_fee, 11_500);
                            continue;
                        } else {
                            report_wallet(reporter.as_ref(),
                                &addr,
                                Some(WalletStatus::Failed),
                                None,
                                None,
                                Some(err_str.clone()),
                            );
                            break (
                                addr,
                                MintResult {
                                    address: addr,
                                    tx_hash: None,
                                    status: WalletStatus::Failed,
                                    gas_used: None,
                                    block_number: None,
                                    error: Some(err_str),
                                },
                            );
                        }
                    }
                };

                log_always(reporter.as_ref(), format!("[{}] SENT t+{}ms tx={}",
                    sign::shorten_address(&addr),
                    mint_started_at.elapsed().as_millis(),
                    sign::shorten_hash(&tx_hash)));

                // Track original + every RBF hash; receipt on any candidate is success.
                let mut candidate_hashes: Vec<B256> = vec![tx_hash];
                let mut mined_hash = tx_hash;
                let mut rbf_count = 0u32;
                const MAX_RBF: u32 = 3;
                const RBF_WAIT_SECS: u64 = 15;
                // RBF bump ×1.30 in basis points.
                const RBF_BUMP_BPS: u64 = 13_000;

                let receipt_result = loop {
                    let wait = if rbf_count < MAX_RBF { RBF_WAIT_SECS } else { 75 };
                    match rpc.wait_for_any_receipt(&candidate_hashes, wait).await {
                        Ok((h, r)) => {
                            mined_hash = h;
                            break Ok(r);
                        }
                        Err(_) if rbf_count < MAX_RBF => {
                            rbf_count += 1;
                            max_fee = gas::bump_fee_bps(max_fee, RBF_BUMP_BPS);
                            max_priority_fee = gas::bump_fee_bps(max_priority_fee, RBF_BUMP_BPS);
                            log_always(reporter.as_ref(), format!("[{}] RBF #{} ({}s pending) gas->{} gwei candidates={}",
                                sign::shorten_address(&addr),
                                rbf_count,
                                rbf_count * RBF_WAIT_SECS as u32,
                                max_fee / U256::from(1_000_000_000u64),
                                candidate_hashes.len()));
                            let rbf_tx = sign::BuiltTx {
                                chain_id,
                                nonce,
                                to: to_addr,
                                value: tx_value,
                                data: tx.data.clone(),
                                gas_limit,
                                max_fee,
                                max_priority_fee,
                            };
                            if let Ok((rbf_raw, rbf_hash)) = sign::sign_transaction(&signer, &rbf_tx) {
                                match rpc.race_send(&rbf_raw).await {
                                    Ok(_) => {
                                        if !candidate_hashes.contains(&rbf_hash) {
                                            candidate_hashes.push(rbf_hash);
                                        }
                                    }
                                    Err(ref e) if is_already_known(&format!("{}", e)) => {
                                        if !candidate_hashes.contains(&rbf_hash) {
                                            candidate_hashes.push(rbf_hash);
                                        }
                                    }
                                    Err(_) => {}
                                }
                            }
                            continue;
                        }
                        Err(e) => break Err(e),
                    }
                };

                match receipt_result {
                    Ok(receipt) => {
                        let info = rpc::parse_receipt(&receipt);
                        if info.success {
                            mint_log(reporter.as_ref(), quiet_w,
                                format!(
                                    "[{}] CONFIRMED t+{}ms gas={} block={} tx={}",
                                    sign::shorten_address(&addr),
                                    mint_started_at.elapsed().as_millis(),
                                    info.gas_used,
                                    info.block_number,
                                    sign::shorten_hash(&mined_hash)
                                ),
                            );
                            maybe_beep(beep_w, &first_confirm_w);
                            report_wallet(reporter.as_ref(),
                                &addr,
                                Some(WalletStatus::Confirmed),
                                Some(format!("gas={} blk={}", info.gas_used, info.block_number)),
                                Some(mined_hash),
                                None,
                            );
                            break (
                                addr,
                                MintResult {
                                    address: addr,
                                    // Mined hash may be original or any RBF replacement.
                                    tx_hash: Some(mined_hash),
                                    status: WalletStatus::Confirmed,
                                    gas_used: Some(info.gas_used),
                                    block_number: Some(info.block_number),
                                    error: None,
                                },
                            );
                        } else {
                            log_always(reporter.as_ref(), format!("[{}] REVERTED t+{}ms gas={} block={} tx={}",
                                sign::shorten_address(&addr),
                                mint_started_at.elapsed().as_millis(),
                                info.gas_used,
                                info.block_number,
                                sign::shorten_hash(&mined_hash)));
                            report_wallet(reporter.as_ref(),
                                &addr,
                                Some(WalletStatus::Failed),
                                Some(format!("reverted blk={}", info.block_number)),
                                Some(mined_hash),
                                Some("reverted".into()),
                            );
                            break (
                                addr,
                                MintResult {
                                    address: addr,
                                    tx_hash: Some(mined_hash),
                                    status: WalletStatus::Failed,
                                    gas_used: Some(info.gas_used),
                                    block_number: Some(info.block_number),
                                    error: Some("reverted".to_string()),
                                },
                            );
                        }
                    }
                    Err(e) => {
                        let err = format!("receipt: {}", e);
                        report_wallet(reporter.as_ref(),
                            &addr,
                            Some(WalletStatus::Sent),
                            Some("receipt timeout".into()),
                            Some(mined_hash),
                            Some(err.clone()),
                        );
                        break (
                            addr,
                            MintResult {
                                address: addr,
                                tx_hash: Some(mined_hash),
                                status: WalletStatus::Sent,
                                gas_used: None,
                                block_number: None,
                                error: Some(err),
                            },
                        );
                    }
                }
            }
        });
    }

    let n_workers = handles.len();
    log_always(
        reporter.as_ref(),
        format!("Minting with {} wallet worker(s)...", n_workers),
    );
    report_phase(
        reporter.as_ref(),
        "confirm",
        format!("Waiting for confirmations ({n_workers} wallet(s))…"),
    );

    let mut results: Vec<MintResult> = Vec::new();
    while let Some(res) = handles.join_next().await {
        match res {
            Ok((_addr, result)) => {
                report_wallet(
                    reporter.as_ref(),
                    &result.address,
                    Some(result.status),
                    Some(format!("gas={:?}", result.gas_used)),
                    result.tx_hash,
                    result.error.clone(),
                );
                results.push(result);
            }
            Err(e) => {
                log_always(reporter.as_ref(), format!("Task panicked: {}", e));
            }
        }
    }

    // Flashbots coordinator: sim or send bundle, then receipt poll for pending pieces.
    if use_flashbots {
        let pieces = fb_pieces.lock().map(|g| g.clone()).unwrap_or_default();
        if pieces.is_empty() {
            log_always(
                reporter.as_ref(),
                "Flashbots: no signed pieces (all prep/sim failed)".to_string(),
            );
        } else {
            let fb_cfg = FlashbotsConfig::from_env(env);
            match FlashbotsClient::new(fb_cfg) {
                Ok(client) => {
                    let auth = &signers[0];
                    let current = rpc.block_number().await.unwrap_or(0);
                    if dry_run {
                        let target = current.saturating_add(1);
                        log_always(
                            reporter.as_ref(),
                            format!(
                                "Flashbots eth_callBundle: {} tx(s) @ block {}",
                                pieces.len(),
                                target
                            ),
                        );
                        match client.call_bundle(auth, &pieces, target).await {
                            Ok(res) => {
                                let errs = flashbots::call_bundle_errors(&res);
                                log_always(
                                    reporter.as_ref(),
                                    format!("callBundle result: {res}"),
                                );
                                for (i, p) in pieces.iter().enumerate() {
                                    if let Some(Some(err)) = errs.get(i) {
                                        if let Some(r) =
                                            results.iter_mut().find(|r| r.address == p.from)
                                        {
                                            r.status = WalletStatus::Failed;
                                            r.error = Some(format!("callBundle: {err}"));
                                        }
                                    } else if let Some(r) =
                                        results.iter_mut().find(|r| r.address == p.from)
                                    {
                                        r.status = WalletStatus::DryRunOk;
                                        r.error =
                                            Some("sim OK (callBundle) — not submitted".into());
                                    }
                                }
                            }
                            Err(e) => {
                                log_always(
                                    reporter.as_ref(),
                                    format!("callBundle failed: {e}"),
                                );
                                for r in results.iter_mut() {
                                    if r.error.as_deref() == Some("__flashbots_dry__") {
                                        r.status = WalletStatus::Failed;
                                        r.error = Some(format!("sim FAIL (callBundle): {e}"));
                                    }
                                }
                            }
                        }
                    } else {
                        log_always(
                            reporter.as_ref(),
                            format!(
                                "Flashbots eth_sendBundle: {} tx(s) from block {}",
                                pieces.len(),
                                current
                            ),
                        );
                        match client
                            .send_bundle_window(auth, &pieces, current, cancel.clone())
                            .await
                        {
                            Ok(sub) => {
                                log_always(
                                    reporter.as_ref(),
                                    format!(
                                        "submitted targets={:?} hash={:?}",
                                        sub.target_blocks, sub.bundle_hash
                                    ),
                                );
                                for r in results.iter_mut() {
                                    if r.error.as_deref() == Some("__flashbots_pending__") {
                                        r.error = Some("submitted — waiting inclusion".into());
                                    }
                                }
                            }
                            Err(e) => {
                                log_always(
                                    reporter.as_ref(),
                                    format!("sendBundle failed: {e}"),
                                );
                                for r in results.iter_mut() {
                                    if r.error.as_deref() == Some("__flashbots_pending__") {
                                        r.status = WalletStatus::Failed;
                                        r.error = Some(format!("submit FAIL: {e}"));
                                    }
                                }
                            }
                        }
                        // Receipt poll for pending bundle wallets
                        for p in &pieces {
                            if cancelled(&cancel) {
                                break;
                            }
                            let Some(r) = results.iter_mut().find(|r| r.address == p.from) else {
                                continue;
                            };
                            if r.status == WalletStatus::Failed
                                && r.error
                                    .as_ref()
                                    .map(|e| e.starts_with("submit FAIL"))
                                    .unwrap_or(false)
                            {
                                continue;
                            }
                            if r.error.as_deref() != Some("__flashbots_pending__")
                                && r.error.as_deref() != Some("submitted — waiting inclusion")
                                && r.status != WalletStatus::Sent
                            {
                                continue;
                            }
                            match rpc.wait_for_receipt(&p.tx_hash, 90).await {
                                Ok(receipt) => {
                                    let info = rpc::parse_receipt(&receipt);
                                    if info.success {
                                        r.status = WalletStatus::Confirmed;
                                        r.gas_used = Some(info.gas_used);
                                        r.block_number = Some(info.block_number);
                                        r.tx_hash = Some(p.tx_hash);
                                        r.error = Some("confirmed".into());
                                        maybe_beep(beep, &first_confirm);
                                        report_wallet(
                                            reporter.as_ref(),
                                            &p.from,
                                            Some(WalletStatus::Confirmed),
                                            Some(format!("confirmed block={}", info.block_number)),
                                            Some(p.tx_hash),
                                            None,
                                        );
                                    } else {
                                        r.status = WalletStatus::Failed;
                                        r.error = Some("included but reverted".into());
                                        r.tx_hash = Some(p.tx_hash);
                                    }
                                }
                                Err(e) => {
                                    r.status = WalletStatus::Sent;
                                    r.error = Some(format!(
                                        "submitted — not included ({e})"
                                    ));
                                    r.tx_hash = Some(p.tx_hash);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    log_always(
                        reporter.as_ref(),
                        format!("Flashbots client error: {e}"),
                    );
                    for r in results.iter_mut() {
                        if matches!(
                            r.error.as_deref(),
                            Some("__flashbots_pending__") | Some("__flashbots_dry__")
                        ) {
                            r.status = WalletStatus::Failed;
                            r.error = Some(format!("flashbots: {e}"));
                        }
                    }
                }
            }
        }
        // Strip internal markers
        for r in results.iter_mut() {
            if matches!(
                r.error.as_deref(),
                Some("__flashbots_pending__") | Some("__flashbots_dry__")
            ) {
                r.error = None;
            }
        }
    }

    let elapsed = mint_started_at.elapsed().as_millis() as u64;
    // Success only after on-chain confirm (or dry-run OK). SENT is not enough.
    let confirmed = results
        .iter()
        .filter(|r| {
            matches!(
                r.status,
                WalletStatus::Confirmed | WalletStatus::DryRunOk
            )
        })
        .count();
    let failed = results
        .iter()
        .filter(|r| matches!(r.status, WalletStatus::Failed))
        .count();
    report_phase(
        reporter.as_ref(),
        "done",
        format!("Done: {confirmed} ok · {failed} fail · {elapsed}ms"),
    );
    let total = results.len();
    log_always(
        reporter.as_ref(),
        format!(
            "Done: {}/{} ok, {} failed, total={}ms",
            confirmed, total, failed, elapsed
        ),
    );
    if let Some(ref p) = _mint_log_path {
        log_always(reporter.as_ref(), format!("Log file: {p}"));
    }

    let mut export_json = None;
    let mut export_csv = None;
    if do_export {
        let run = export::MintRunExport {
            slug: info.slug.clone(),
            chain: info.chain.clone(),
            phase: stage_type_owned.clone(),
            started_at: chrono::Utc::now().to_rfc3339(),
            elapsed_ms: elapsed,
            quiet,
            skip_preflight,
            dry_run,
            wallets: results
                .iter()
                .map(|w| {
                    export::wallet_row(
                        w.address,
                        w.status,
                        w.tx_hash,
                        w.gas_used,
                        w.block_number,
                        w.error.clone(),
                        None,
                    )
                })
                .collect(),
            confirmed,
            failed,
            total,
        };
        match export::write_mint_results(&run) {
            Ok((json_p, csv_p)) => {
                export_json = Some(export::path_display(&json_p));
                export_csv = Some(export::path_display(&csv_p));
                log_always(
                    reporter.as_ref(),
                    format!(
                        "Results exported: {} | {}",
                        export_json.as_deref().unwrap_or("-"),
                        export_csv.as_deref().unwrap_or("-")
                    ),
                );
            }
            Err(e) => log_always(reporter.as_ref(), format!("Export failed: {}", e)),
        }
    }

    let wallets: Vec<crate::api::SweepResultRow> = results
        .iter()
        .map(|r| crate::api::SweepResultRow {
            address: format!("{:?}", r.address),
            status: r.status.to_string(),
            tx_hash: r.tx_hash.map(|h| format!("0x{}", hex::encode(h.as_slice()))),
            gas_used: r.gas_used,
            block_number: r.block_number,
            error: r.error.clone(),
        })
        .collect();

    Ok(MintRunSummary {
        slug: info.slug,
        chain: info.chain,
        phase: stage_type_owned,
        dry_run,
        elapsed_ms: elapsed,
        results,
        confirmed,
        failed,
        export_json,
        export_csv,
        wallets,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        classify_mint_error, fire_lag_ms_from_clock, parse_tx_calldata_hex, resolve_mint_gas_limit,
    };

    #[test]
    fn fire_lag_uses_millis_not_seconds() {
        // open at t=1000s, now = 1000s + 250ms
        assert_eq!(fire_lag_ms_from_clock(1000, 1_000_250), 250);
        // exactly on open
        assert_eq!(fire_lag_ms_from_clock(1000, 1_000_000), 0);
        // slightly early → 0
        assert_eq!(fire_lag_ms_from_clock(1000, 999_900), 0);
        // 1.5s late
        assert_eq!(fire_lag_ms_from_clock(1000, 1_001_500), 1500);
    }

    #[test]
    fn parse_at_time_invalid_is_err_not_silent() {
        // Core schedule path uses the same helper — invalid must error.
        assert!(crate::mint_ops::parse_at_time_unix("not-a-time").is_err());
        assert!(crate::mint_ops::parse_at_time_unix("2020-13-40T99:99:99Z").is_err());
        assert_eq!(
            crate::mint_ops::parse_at_time_unix("1700000000").unwrap(),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn parse_calldata_rejects_empty() {
        assert!(parse_tx_calldata_hex("").is_err());
        assert!(parse_tx_calldata_hex("0x").is_err());
        assert!(parse_tx_calldata_hex("   ").is_err());
    }

    #[test]
    fn parse_calldata_rejects_invalid_hex() {
        assert!(parse_tx_calldata_hex("0xzz").is_err());
        assert!(parse_tx_calldata_hex("not-hex").is_err());
    }

    #[test]
    fn parse_calldata_ok_selector() {
        let b = parse_tx_calldata_hex("0xa0712d68").unwrap();
        assert_eq!(b.as_ref(), &[0xa0, 0x71, 0x2d, 0x68]);
        let b2 = parse_tx_calldata_hex("a0712d68").unwrap();
        assert_eq!(b.as_ref(), b2.as_ref());
    }

    #[test]
    fn mint_gas_estimate_applies_l2_floor() {
        // Base (8453): raw 21k estimate must floor to >= 150k
        let lim = resolve_mint_gas_limit(21_000, 1.15, 8453, false);
        assert!(lim >= 150_000, "lim={lim}");
        // Ethereum mainnet: no 150k floor
        let eth = resolve_mint_gas_limit(21_000, 1.15, 1, false);
        assert!(eth >= 21_000);
        assert!(eth < 150_000);
    }

    #[test]
    fn mint_gas_fixed_clamps_l2_floor() {
        let lim = resolve_mint_gas_limit(50_000, 1.0, 8453, true);
        assert_eq!(lim, 150_000);
        let eth = resolve_mint_gas_limit(50_000, 1.0, 1, true);
        assert_eq!(eth, 50_000);
    }

    #[test]
    fn generic_execution_reverted_is_retryable() {
        assert_eq!(classify_mint_error("execution reverted"), "retryable");
        assert_eq!(
            classify_mint_error("Error: execution reverted: unknown reason"),
            "retryable"
        );
        assert_eq!(
            classify_mint_error("RPC eth_estimateGas error: {\"message\":\"execution reverted\"}"),
            "retryable"
        );
    }

    #[test]
    fn known_contract_errors_are_fatal() {
        assert_eq!(classify_mint_error("execution reverted: InvalidProof"), "fatal");
        assert_eq!(classify_mint_error("IncorrectPayment"), "fatal");
        assert_eq!(classify_mint_error("MintQuantityExceedsMaxSupply"), "fatal");
        assert_eq!(
            classify_mint_error("insufficient funds for gas * price + value"),
            "fatal"
        );
        assert_eq!(
            classify_mint_error(
                "RPC eth_estimateGas error: {\"code\":-32003,\"message\":\"EVM error: OutOfFunds\"}"
            ),
            "fatal"
        );
        assert_eq!(classify_mint_error("SignatureAlreadyUsed()"), "fatal");
        assert_eq!(classify_mint_error("PayerNotAllowed"), "fatal");
        assert_eq!(
            classify_mint_error("MintQuantityExceedsMaxMintedPerWallet"),
            "fatal"
        );
    }

    #[test]
    fn temporary_or_unknown_errors_are_retryable() {
        assert_eq!(classify_mint_error("timeout"), "retryable");
        assert_eq!(classify_mint_error("nonce too low"), "retryable");
        assert_eq!(classify_mint_error("replacement transaction underpriced"), "retryable");
        assert_eq!(classify_mint_error(""), "retryable");
    }
}