//! Raw contract sniper — **pre-sign race** (FCFS / L2 sequencer path).
//!
//! Architecture:
//! 1. Resolve value + hard gas limit (no estimate at fire)
//! 2. Wait until `at_time − prep_lead`, then **pre-sign** all wallets (nonce + sign)
//! 3. Clock-fire at `at_time` → parallel `eth_sendRawTransaction` blast
//! 4. Receipts after send (not on the hot path)
//!
//! Does **not** gate on `getMintStatus` / `estimate_gas` at open.
//! Probe helpers remain for UI status only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::abi::build_calldata;
use crate::gas;
use crate::mint_ops::parse_at_time_unix;
use crate::progress::{MintEvent, MintReporter};
use crate::rpc::RpcClient;
use crate::safety_policy::{should_refresh_fees_at_fire, FeeRefreshMode};
use crate::sign::{sign_transaction, shorten_hash, BuiltTx};
use crate::types::{GasParams, MintResult, Signer, WalletStatus};

/// Default gas limit when UI leaves it empty (Hoodies winners used ~450k–650k).
const DEFAULT_GAS_LIMIT: u64 = 650_000;
/// Start pre-sign this many seconds before `at_time`.
const PREP_LEAD_SECS: i64 = 5;

// ─── Public config ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SniperPreset {
    /// MintBay Generative V3/V4 public: `mint(uint256)` + Auto value via `getMintStatus`.
    #[default]
    MintBayPublic,
    /// Plain `mint(uint256)` — fire at `at_time` (or immediately).
    SimpleMintUint,
    /// User function signature + params.
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ValueMode {
    /// MintBay: (phase.mintPrice + collectorFee) * qty. Else falls back to fixed.
    #[default]
    Auto,
    Fixed,
}

#[derive(Clone)]
pub struct RawSniperConfig {
    pub contract: Address,
    pub preset: SniperPreset,
    /// Call signature for mint tx (e.g. `mint(uint256)`).
    pub function: String,
    /// Static params; for `mint(uint256)` use empty and set `quantity`.
    pub params: Vec<String>,
    pub quantity: u64,
    pub value_mode: ValueMode,
    /// Used when Fixed, or Auto fallback if MintBay status fails.
    pub fixed_value: U256,
    pub gas: GasParams,
    pub dry_run: bool,
    /// Unix seconds (optional). None → fire immediately after pre-sign.
    pub at_time: Option<i64>,
    /// Max wait until prep window (at_time − lead); also bounds hang waits.
    pub timeout_secs: u64,
    pub concurrency: usize,
    /// Optional hard gas limit per tx (from UI). Default applied in runner.
    pub gas_limit: Option<u64>,
    /// When to re-fetch fees + re-sign at fire (default mainnet-only).
    pub fee_refresh: FeeRefreshMode,
}

impl Default for RawSniperConfig {
    fn default() -> Self {
        Self {
            contract: Address::ZERO,
            preset: SniperPreset::MintBayPublic,
            function: "mint(uint256)".into(),
            params: vec![],
            quantity: 1,
            value_mode: ValueMode::Auto,
            fixed_value: U256::ZERO,
            gas: GasParams::default(),
            dry_run: false,
            at_time: None,
            timeout_secs: 300,
            concurrency: 16,
            gas_limit: None,
            fee_refresh: FeeRefreshMode::MainnetOnly,
        }
    }
}

// ─── Decode helpers (MintBay status) ─────────────────────────────────────────

fn word_u256(data: &[u8], index: usize) -> Result<U256> {
    let start = index * 32;
    let end = start + 32;
    if data.len() < end {
        bail!("eth_call result too short for word {index}");
    }
    Ok(U256::from_be_slice(&data[start..end]))
}

fn word_bool(data: &[u8], index: usize) -> Result<bool> {
    Ok(!word_u256(data, index)?.is_zero())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cancelled(cancel: &Option<Arc<AtomicBool>>) -> bool {
    cancel
        .as_ref()
        .map(|c| c.load(Ordering::SeqCst))
        .unwrap_or(false)
}

fn report(rep: &Option<Arc<dyn MintReporter>>, ev: MintEvent) {
    if let Some(r) = rep {
        r.report(ev);
    }
}

/// Sleep until unix second `target` (or return if already past). Honours cancel.
async fn sleep_until_unix(target: i64, cancel: &Option<Arc<AtomicBool>>) -> Result<(), String> {
    loop {
        if cancelled(cancel) {
            return Err("cancelled by user".into());
        }
        let now = now_unix();
        let rem = target - now;
        if rem <= 0 {
            return Ok(());
        }
        let ms = if rem > 5 {
            500
        } else if rem > 1 {
            50
        } else {
            5
        };
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

/// Fine-grained wait for fire: target is unix **seconds**; spins last ~20ms.
async fn sleep_until_fire(target_unix: i64, cancel: &Option<Arc<AtomicBool>>) -> Result<(), String> {
    let target_ms = target_unix.saturating_mul(1000);
    loop {
        if cancelled(cancel) {
            return Err("cancelled by user".into());
        }
        let now = now_unix_ms();
        let rem = target_ms - now;
        if rem <= 0 {
            return Ok(());
        }
        if rem > 2_000 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else if rem > 50 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        } else if rem > 5 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        } else {
            // busy-ish spin for last few ms
            tokio::task::yield_now().await;
        }
    }
}

async fn resolve_mint_value(rpc: &RpcClient, config: &RawSniperConfig) -> (U256, String) {
    match config.value_mode {
        ValueMode::Fixed => (
            config.fixed_value,
            format!("fixed {} wei", config.fixed_value),
        ),
        ValueMode::Auto => {
            if matches!(config.preset, SniperPreset::MintBayPublic) {
                match fetch_mintbay_status(rpc, &config.contract).await {
                    Ok(st) => {
                        let v = st.mint_value(config.quantity);
                        (
                            v,
                            format!(
                                "MintBay auto {} wei (phaseType={} minted={}/{})",
                                v, st.current_phase_type, st.total_minted, st.max_supply
                            ),
                        )
                    }
                    Err(e) => (
                        config.fixed_value,
                        format!(
                            "MintBay status fail ({e}) — using fixed {} wei",
                            config.fixed_value
                        ),
                    ),
                }
            } else {
                (
                    config.fixed_value,
                    format!("auto→fixed {} wei", config.fixed_value),
                )
            }
        }
    }
}

struct PreSigned {
    address: Address,
    raw: Bytes,
    hash: B256,
    /// Needed to re-sign on L1 if fees rise between prep and fire.
    nonce: u64,
}

// ─── MintBay status ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MintBayStatus {
    pub public_mint_price: U256,
    pub max_supply: U256,
    pub total_minted: U256,
    pub collector_fee: U256,
    pub resolved_phase_id: U256,
    pub minting_paused: bool,
    pub current_phase_type: u8,
    pub phase_start: U256,
    pub phase_end: U256,
    pub phase_mint_price: U256,
}

impl MintBayStatus {
    /// Public mint open for sniping.
    pub fn is_public_open(&self, wall_now: i64) -> bool {
        if self.minting_paused {
            return false;
        }
        // 0=Paused 1=Allowlist 2=Public
        if self.current_phase_type != 2 {
            return false;
        }
        if self.resolved_phase_id.is_zero() {
            return false;
        }
        if !self.max_supply.is_zero() && self.total_minted >= self.max_supply {
            return false;
        }
        let start = self.phase_start.to::<u128>() as i64;
        let end = self.phase_end.to::<u128>() as i64;
        if start > 0 && wall_now < start {
            return false;
        }
        if end > 0 && wall_now > end {
            return false;
        }
        true
    }

    pub fn mint_value(&self, qty: u64) -> U256 {
        let price = if !self.resolved_phase_id.is_zero() {
            self.phase_mint_price
        } else {
            self.public_mint_price
        };
        let per = price.saturating_add(self.collector_fee);
        per.saturating_mul(U256::from(qty.max(1)))
    }
}

/// `getMintStatus()` selector 0x941ada0e — flat static ABI layout (17 words).
/// Falls back to individual view calls if the combined call fails (RPC / proxy quirks).
pub async fn fetch_mintbay_status(rpc: &RpcClient, contract: &Address) -> Result<MintBayStatus> {
    let data = Bytes::from(hex::decode("941ada0e").context("sel")?);
    match rpc.eth_call(&Address::ZERO, contract, &data).await {
        Ok(raw) if raw.len() >= 17 * 32 => Ok(MintBayStatus {
            public_mint_price: word_u256(&raw, 1)?,
            max_supply: word_u256(&raw, 2)?,
            total_minted: word_u256(&raw, 3)?,
            collector_fee: word_u256(&raw, 4)?,
            resolved_phase_id: word_u256(&raw, 5)?,
            minting_paused: word_bool(&raw, 7)?,
            current_phase_type: word_u256(&raw, 8)?.to::<u8>(),
            phase_start: word_u256(&raw, 10)?,
            phase_end: word_u256(&raw, 11)?,
            phase_mint_price: word_u256(&raw, 12)?,
        }),
        Ok(raw) if !raw.is_empty() => {
            // Unexpected short return — try views
            crate::rlog!(
                "getMintStatus short return ({} bytes), using view fallback",
                raw.len()
            );
            fetch_mintbay_status_fallback(rpc, contract).await
        }
        Ok(_) => {
            crate::rlog!("getMintStatus empty return, using view fallback");
            fetch_mintbay_status_fallback(rpc, contract).await
        }
        Err(e) => {
            crate::rlog!("getMintStatus eth_call failed ({e}), using view fallback");
            match fetch_mintbay_status_fallback(rpc, contract).await {
                Ok(st) if !st.max_supply.is_zero() || !st.collector_fee.is_zero() => Ok(st),
                Ok(_) => Err(e).context("getMintStatus eth_call (fallback also empty)"),
                Err(e2) => Err(e).context(format!("getMintStatus eth_call; fallback: {e2}")),
            }
        }
    }
}

async fn view_u256(rpc: &RpcClient, contract: &Address, sel_hex: &str) -> U256 {
    let Ok(bytes) = hex::decode(sel_hex) else {
        return U256::ZERO;
    };
    let data = Bytes::from(bytes);
    match rpc.eth_call(&Address::ZERO, contract, &data).await {
        Ok(raw) if raw.len() >= 32 => word_u256(&raw, 0).unwrap_or(U256::ZERO),
        _ => U256::ZERO,
    }
}

async fn fetch_mintbay_status_fallback(
    rpc: &RpcClient,
    contract: &Address,
) -> Result<MintBayStatus> {
    Ok(MintBayStatus {
        public_mint_price: U256::ZERO,
        max_supply: view_u256(rpc, contract, "d5abeb01").await,
        total_minted: view_u256(rpc, contract, "18160ddd").await,
        collector_fee: view_u256(rpc, contract, "f103eaaf").await,
        resolved_phase_id: view_u256(rpc, contract, "40c5b34e").await,
        minting_paused: !view_u256(rpc, contract, "e1a283d6").await.is_zero(),
        current_phase_type: view_u256(rpc, contract, "055ad42e").await.to::<u8>(),
        phase_start: U256::ZERO,
        phase_end: U256::ZERO,
        phase_mint_price: U256::ZERO,
    })
}

fn build_mint_params(config: &RawSniperConfig) -> Result<Vec<String>> {
    if config.function.contains("uint256") && config.params.is_empty() {
        // mint(uint256) with quantity
        return Ok(vec![config.quantity.max(1).to_string()]);
    }
    if !config.params.is_empty() {
        return Ok(config.params.clone());
    }
    // zero-arg
    if config.function.contains("()") && !config.function.contains(',') {
        return Ok(vec![]);
    }
    Ok(vec![config.quantity.max(1).to_string()])
}

// ─── Main entry (pre-sign race) ───────────────────────────────────────────────

pub async fn run_raw_sniper(
    signers: &[Signer],
    rpc: &RpcClient,
    config: &RawSniperConfig,
    cancel: Option<Arc<AtomicBool>>,
    reporter: Option<Arc<dyn MintReporter>>,
) -> Vec<MintResult> {
    if signers.is_empty() {
        return vec![];
    }

    let params = match build_mint_params(config) {
        Ok(p) => p,
        Err(e) => return fail_all(signers, format!("params: {e}")),
    };
    let calldata = match build_calldata(&config.function, &params) {
        Ok(c) => c,
        Err(e) => return fail_all(signers, format!("calldata: {e}")),
    };

    let chain_id = rpc.chain_id().await.unwrap_or(1);
    let (base_fee, network_priority) = rpc
        .fee_history()
        .await
        .unwrap_or((U256::from(1_000_000_000u64), U256::from(1_000_000_000u64)));
    let (mut max_fee, mut max_priority_fee) =
        match gas::calculate_fees(&config.gas, base_fee, network_priority) {
            Ok(f) => f,
            Err(e) => return fail_all(signers, format!("gas: {e}")),
        };

    let gas_limit = config
        .gas_limit
        .filter(|&g| g >= 21_000)
        .unwrap_or(DEFAULT_GAS_LIMIT);
    let concurrency = config.concurrency.max(1);
    let fire_at = config.at_time;
    let start_wall = now_unix();

    // Hard deadline only for hanging waits (not open-poll).
    let wait_deadline = match fire_at {
        Some(at) => at.saturating_add(config.timeout_secs.max(60) as i64),
        None => start_wall.saturating_add(config.timeout_secs.max(60) as i64),
    };

    report(
        &reporter,
        MintEvent::phase(
            "wait",
            format!(
                "PRE-SIGN RACE · contract={:?} qty={} gas_limit={} wallets={} at_time={:?}",
                config.contract,
                config.quantity,
                gas_limit,
                signers.len(),
                fire_at
            ),
        ),
    );
    report(
        &reporter,
        MintEvent::message(format!(
            "Mode: pre-sign → clock fire → blast | dry_run={} | no estimate/getMintStatus at T0",
            config.dry_run
        )),
    );

    // ── Wait until prep window (at_time − PREP_LEAD) ──
    if let Some(at) = fire_at {
        let prep_at = at.saturating_sub(PREP_LEAD_SECS);
        if now_unix() < prep_at {
            report(
                &reporter,
                MintEvent::message(format!(
                    "Waiting until prep T−{PREP_LEAD_SECS}s (fire at {at}, now {})…",
                    now_unix()
                )),
            );
            // Log every ~10s while waiting
            loop {
                if cancelled(&cancel) {
                    return fail_all(signers, "cancelled by user");
                }
                let now = now_unix();
                if now >= prep_at {
                    break;
                }
                if now > wait_deadline {
                    return fail_all(signers, "timeout before prep window");
                }
                let left = prep_at - now;
                if left % 10 == 0 || left <= 5 {
                    report(
                        &reporter,
                        MintEvent::message(format!("clock: prep in {left}s · fire in {}s", at - now)),
                    );
                }
                if let Err(e) = sleep_until_unix((now + 1).min(prep_at), &cancel).await {
                    return fail_all(signers, e);
                }
            }
        }
    }

    if cancelled(&cancel) {
        return fail_all(signers, "cancelled by user");
    }

    // ── Resolve value at prep time (MintBay auto once — not a fire gate) ──
    let (value, value_detail) = resolve_mint_value(rpc, config).await;
    report(
        &reporter,
        MintEvent::message(format!("Value: {value_detail}")),
    );

    // ── PRE-SIGN all wallets ──
    report(
        &reporter,
        MintEvent::phase(
            "prep",
            format!(
                "Pre-signing {} wallet(s) · gas_limit={gas_limit} · value={value} wei",
                signers.len()
            ),
        ),
    );

    let sem = Arc::new(Semaphore::new(concurrency));
    let mut prep_handles = Vec::new();

    for signer in signers.iter() {
        let signer = signer.clone();
        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let rpc = rpc.clone();
        let calldata = calldata.clone();
        let contract = config.contract;
        let rep = reporter.clone();
        let cancel_w = cancel.clone();

        prep_handles.push(tokio::spawn(async move {
            let _permit = permit;
            let addr = signer.address();
            let fail = |e: String| Err((addr, e));

            if cancelled(&cancel_w) {
                return fail("cancelled by user".into());
            }

            report(
                &rep,
                MintEvent::wallet(
                    addr,
                    Some(WalletStatus::Wait),
                    Some("pre-sign".into()),
                    None,
                    None,
                ),
            );

            let nonce = match rpc.nonce(&addr).await {
                Ok(n) => n,
                Err(e) => return fail(format!("nonce: {e}")),
            };

            let tx = BuiltTx {
                chain_id,
                nonce,
                to: contract,
                value,
                data: calldata,
                gas_limit,
                max_fee,
                max_priority_fee,
            };

            let (raw, hash) = match sign_transaction(&signer, &tx) {
                Ok(x) => x,
                Err(e) => return fail(format!("sign: {e}")),
            };

            report(
                &rep,
                MintEvent::wallet(
                    addr,
                    Some(WalletStatus::Sim),
                    Some(format!(
                        "signed {} nonce={nonce} gas={gas_limit}",
                        shorten_hash(&hash)
                    )),
                    Some(hash),
                    None,
                ),
            );

            Ok(PreSigned {
                address: addr,
                raw,
                hash,
                nonce,
            })
        }));
    }

    let mut prepared: Vec<PreSigned> = Vec::new();
    let mut prep_fails: Vec<MintResult> = Vec::new();
    for h in prep_handles {
        match h.await {
            Ok(Ok(ps)) => prepared.push(ps),
            Ok(Err((addr, e))) => {
                prep_fails.push(MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::Failed,
                    gas_used: None,
                    block_number: None,
                    error: Some(e),
                });
            }
            Err(e) => {
                crate::rlog!("pre-sign task join error: {e}");
            }
        }
    }

    if prepared.is_empty() {
        report(
            &reporter,
            MintEvent::phase("error", "Pre-sign failed for all wallets"),
        );
        return prep_fails;
    }

    report(
        &reporter,
        MintEvent::message(format!(
            "Pre-signed {}/{} · ready to fire",
            prepared.len(),
            signers.len()
        )),
    );

    // Dry-run: stop after sign (no broadcast)
    if config.dry_run {
        let mut results = prep_fails;
        for ps in prepared {
            report(
                &reporter,
                MintEvent::wallet(
                    ps.address,
                    Some(WalletStatus::DryRunOk),
                    Some(format!("pre-signed {}", shorten_hash(&ps.hash))),
                    Some(ps.hash),
                    None,
                ),
            );
            results.push(MintResult {
                address: ps.address,
                tx_hash: Some(ps.hash),
                status: WalletStatus::DryRunOk,
                gas_used: Some(gas_limit),
                block_number: None,
                error: None,
            });
        }
        report(
            &reporter,
            MintEvent::phase("done", format!("Dry-run pre-sign OK: {}/{}", results.len(), signers.len())),
        );
        return results;
    }

    // ── Clock fire ──
    if let Some(at) = fire_at {
        let now = now_unix();
        if now < at {
            report(
                &reporter,
                MintEvent::phase(
                    "wait",
                    format!("Armed — firing in {}s (clock {at})", at - now),
                ),
            );
            if let Err(e) = sleep_until_fire(at, &cancel).await {
                return fail_all(signers, e);
            }
        } else {
            report(
                &reporter,
                MintEvent::message(format!(
                    "at_time already passed by {}s — firing immediately",
                    now - at
                )),
            );
        }
    }

    if cancelled(&cancel) {
        return fail_all(signers, "cancelled by user");
    }

    // Fee refresh at fire: default MainnetOnly (no L2 latency); Always/Never via config.
    if should_refresh_fees_at_fire(chain_id, config.fee_refresh) {
        match rpc.fee_history().await {
            Ok((base, prio)) => match gas::calculate_fees(&config.gas, base, prio) {
                Ok((mf, pf)) => {
                    let bump_fee = mf > max_fee;
                    let bump_prio = pf > max_priority_fee;
                    if bump_fee || bump_prio {
                        let old_fee = max_fee;
                        let old_prio = max_priority_fee;
                        if bump_fee {
                            max_fee = mf;
                        }
                        if bump_prio {
                            max_priority_fee = pf;
                        }
                        report(
                            &reporter,
                            MintEvent::message(format!(
                                "fee refresh (mode={}): max_fee {}→{} gwei prio {}→{} gwei — re-signing {}",
                                config.fee_refresh.as_str(),
                                old_fee / U256::from(1_000_000_000u64),
                                max_fee / U256::from(1_000_000_000u64),
                                old_prio / U256::from(1_000_000_000u64),
                                max_priority_fee / U256::from(1_000_000_000u64),
                                prepared.len()
                            )),
                        );
                        let mut resign_ok = 0usize;
                        for ps in prepared.iter_mut() {
                            let Some(signer) =
                                signers.iter().find(|s| s.address() == ps.address)
                            else {
                                continue;
                            };
                            let tx = BuiltTx {
                                chain_id,
                                nonce: ps.nonce,
                                to: config.contract,
                                value,
                                data: calldata.clone(),
                                gas_limit,
                                max_fee,
                                max_priority_fee,
                            };
                            match sign_transaction(signer, &tx) {
                                Ok((raw, hash)) => {
                                    ps.raw = raw;
                                    ps.hash = hash;
                                    resign_ok += 1;
                                }
                                Err(e) => {
                                    report(
                                        &reporter,
                                        MintEvent::message(format!(
                                            "[{:?}] re-sign failed (keeping prep): {e}",
                                            ps.address
                                        )),
                                    );
                                }
                            }
                        }
                        report(
                            &reporter,
                            MintEvent::message(format!(
                                "re-signed {resign_ok}/{}",
                                prepared.len()
                            )),
                        );
                    } else {
                        report(
                            &reporter,
                            MintEvent::message(
                                "fee refresh: no increase — using prep signatures",
                            ),
                        );
                    }
                }
                Err(e) => {
                    report(
                        &reporter,
                        MintEvent::message(format!(
                            "fee refresh calc failed ({e}) — using prep fees"
                        )),
                    );
                }
            },
            Err(e) => {
                report(
                    &reporter,
                    MintEvent::message(format!(
                        "fee_history at fire failed ({e}) — using prep fees"
                    )),
                );
            }
        }
    } else {
        report(
            &reporter,
            MintEvent::message(format!(
                "fee refresh skipped (mode={}, chainId={})",
                config.fee_refresh.as_str(),
                chain_id
            )),
        );
    }

    report(
        &reporter,
        MintEvent::phase(
            "fire",
            format!(
                "BLAST {} pre-signed tx(s) @ t={}",
                prepared.len(),
                now_unix_ms()
            ),
        ),
    );

    // ── Parallel send only ──
    let send_sem = Arc::new(Semaphore::new(concurrency));
    let mut send_handles = Vec::new();

    for ps in prepared {
        let permit = match send_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let rpc = rpc.clone();
        let rep = reporter.clone();

        send_handles.push(tokio::spawn(async move {
            let _permit = permit;
            let addr = ps.address;
            let signed_hash = ps.hash;

            // Send first — only then report Sent with real RPC hash
            match rpc.race_send(&ps.raw).await {
                Ok(tx_hash) => {
                    report(
                        &rep,
                        MintEvent::wallet(
                            addr,
                            Some(WalletStatus::Sent),
                            Some(format!("SEND OK {}", shorten_hash(&tx_hash))),
                            Some(tx_hash),
                            None,
                        ),
                    );
                    // Receipt off hot path
                    match rpc.wait_for_receipt(&tx_hash, 90).await {
                        Ok(receipt) => {
                            let info = crate::rpc::parse_receipt(&receipt);
                            if info.success {
                                report(
                                    &rep,
                                    MintEvent::wallet(
                                        addr,
                                        Some(WalletStatus::Confirmed),
                                        Some(format!("block={}", info.block_number)),
                                        Some(tx_hash),
                                        None,
                                    ),
                                );
                                MintResult {
                                    address: addr,
                                    tx_hash: Some(tx_hash),
                                    status: WalletStatus::Confirmed,
                                    gas_used: Some(info.gas_used),
                                    block_number: Some(info.block_number),
                                    error: None,
                                }
                            } else {
                                report(
                                    &rep,
                                    MintEvent::wallet(
                                        addr,
                                        Some(WalletStatus::Failed),
                                        Some("reverted".into()),
                                        Some(tx_hash),
                                        Some("reverted".into()),
                                    ),
                                );
                                MintResult {
                                    address: addr,
                                    tx_hash: Some(tx_hash),
                                    status: WalletStatus::Failed,
                                    gas_used: Some(info.gas_used),
                                    block_number: Some(info.block_number),
                                    error: Some("reverted".into()),
                                }
                            }
                        }
                        Err(e) => {
                            report(
                                &rep,
                                MintEvent::wallet(
                                    addr,
                                    Some(WalletStatus::Sent),
                                    Some(format!("receipt timeout: {e}")),
                                    Some(tx_hash),
                                    Some(format!("receipt: {e}")),
                                ),
                            );
                            MintResult {
                                address: addr,
                                tx_hash: Some(tx_hash),
                                status: WalletStatus::Sent,
                                gas_used: None,
                                block_number: None,
                                error: Some(format!("receipt: {e}")),
                            }
                        }
                    }
                }
                Err(e) => {
                    // Fall back to local hash only in error text (not as Sent success)
                    report(
                        &rep,
                        MintEvent::wallet(
                            addr,
                            Some(WalletStatus::Failed),
                            Some(format!("send fail {}", shorten_hash(&signed_hash))),
                            None,
                            Some(format!("send: {e}")),
                        ),
                    );
                    MintResult {
                        address: addr,
                        tx_hash: None,
                        status: WalletStatus::Failed,
                        gas_used: None,
                        block_number: None,
                        error: Some(format!("send: {e}")),
                    }
                }
            }
        }));
    }

    let mut results = prep_fails;
    for h in send_handles {
        match h.await {
            Ok(r) => results.push(r),
            Err(e) => crate::rlog!("send task join error: {e}"),
        }
    }

    report(
        &reporter,
        MintEvent::phase(
            "done",
            format!(
                "Race done: {}/{} ok",
                results
                    .iter()
                    .filter(|r| matches!(
                        r.status,
                        WalletStatus::Confirmed | WalletStatus::DryRunOk | WalletStatus::Sent
                    ))
                    .count(),
                results.len()
            ),
        ),
    );

    results
}

fn fail_all(signers: &[Signer], err: impl Into<String>) -> Vec<MintResult> {
    let err = err.into();
    signers
        .iter()
        .map(|s| MintResult {
            address: s.address(),
            tx_hash: None,
            status: WalletStatus::Failed,
            gas_used: None,
            block_number: None,
            error: Some(err.clone()),
        })
        .collect()
}

/// Parse at_time string for API layer.
pub fn parse_sniper_at_time(raw: Option<&str>) -> Result<Option<i64>> {
    match raw {
        None => Ok(None),
        Some(s) => parse_at_time_unix(s).map_err(|e| anyhow::anyhow!(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mintbay_public_open_logic() {
        let mut st = MintBayStatus {
            public_mint_price: U256::ZERO,
            max_supply: U256::from(100),
            total_minted: U256::from(1),
            collector_fee: U256::from(400_000_000_000_000u64),
            resolved_phase_id: U256::from(1),
            minting_paused: false,
            current_phase_type: 2,
            phase_start: U256::ZERO,
            phase_end: U256::ZERO,
            phase_mint_price: U256::ZERO,
        };
        assert!(st.is_public_open(1_700_000_000));
        st.current_phase_type = 1;
        assert!(!st.is_public_open(1_700_000_000));
        st.current_phase_type = 2;
        st.minting_paused = true;
        assert!(!st.is_public_open(1_700_000_000));
        st.minting_paused = false;
        st.total_minted = U256::from(100);
        assert!(!st.is_public_open(1_700_000_000));
    }

    #[test]
    fn mintbay_value_formula() {
        let st = MintBayStatus {
            public_mint_price: U256::ZERO,
            max_supply: U256::from(10),
            total_minted: U256::ZERO,
            collector_fee: U256::from(400_000_000_000_000u64), // 0.0004 eth
            resolved_phase_id: U256::from(1),
            minting_paused: false,
            current_phase_type: 2,
            phase_start: U256::ZERO,
            phase_end: U256::ZERO,
            phase_mint_price: U256::from(1_000_000_000_000_000u64), // 0.001
        };
        // (0.001 + 0.0004) * 2
        assert_eq!(st.mint_value(2), U256::from(2_800_000_000_000_000u64));
    }
}
