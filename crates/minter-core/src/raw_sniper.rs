//! Raw contract sniper: wait for mint open (preset / view rules / sim), then parallel mint.
//!
//! Architecture:
//! - **One** coordinator polls open state
//! - On open → fan-out wallets: estimate → send (no global barrier)
//! - Cancel via shared `AtomicBool` (same pattern as OpenSea mint)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, Bytes, U256};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::abi::build_calldata;
use crate::gas::{self, apply_gas_limit};
use crate::mint_ops::parse_at_time_unix;
use crate::progress::{MintEvent, MintReporter};
use crate::rpc::RpcClient;
use crate::sign::{sign_transaction, shorten_hash, BuiltTx};
use crate::types::{GasParams, MintResult, Signer, WalletStatus};

// ─── Public config ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SniperPreset {
    /// MintBay Generative V3/V4 public: `getMintStatus` + `mint(uint256)`.
    #[default]
    MintBayPublic,
    /// Plain `mint(uint256)` — open via at_time and/or sim-open / custom rules.
    SimpleMintUint,
    /// User function + view rules (+ optional sim-open).
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DecodeKind {
    #[default]
    Uint256,
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CompareOp {
    #[default]
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// One eth_call view condition (all rules AND).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewRule {
    /// Full signature, e.g. `currentPhase()` or `mintingPaused()`.
    pub function: String,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub decode: DecodeKind,
    #[serde(default)]
    pub op: CompareOp,
    pub expected: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ValueMode {
    /// MintBay: (phase.mintPrice + collectorFee) * qty. Custom: fixed only unless views set later.
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
    /// Used when Fixed, or as override floor display; Auto ignores unless Fixed.
    pub fixed_value: U256,
    pub gas: GasParams,
    pub dry_run: bool,
    /// Unix seconds (optional).
    pub at_time: Option<i64>,
    /// If true (default), mint as soon as open even before `at_time`.
    pub mint_before_at_time: bool,
    /// Seconds after at_time (or after Start if no at_time) until wait fails.
    pub timeout_secs: u64,
    /// Treat successful estimate_gas as open (Custom/Simple; probe wallet).
    pub sim_open: bool,
    pub rules: Vec<ViewRule>,
    pub concurrency: usize,
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
            mint_before_at_time: true,
            timeout_secs: 300,
            sim_open: false,
            rules: vec![],
            concurrency: 16,
        }
    }
}

// ─── Decode helpers ──────────────────────────────────────────────────────────

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

fn compare_u256(got: U256, op: CompareOp, expected: &str) -> Result<bool> {
    let exp = parse_u256_expected(expected)?;
    Ok(match op {
        CompareOp::Eq => got == exp,
        CompareOp::Ne => got != exp,
        CompareOp::Gt => got > exp,
        CompareOp::Gte => got >= exp,
        CompareOp::Lt => got < exp,
        CompareOp::Lte => got <= exp,
    })
}

fn parse_u256_expected(s: &str) -> Result<U256> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("true") || s == "1" {
        return Ok(U256::from(1));
    }
    if s.eq_ignore_ascii_case("false") || s == "0" {
        return Ok(U256::ZERO);
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return U256::from_str_radix(hex, 16).context("invalid hex expected");
    }
    U256::from_str_radix(s, 10).context("invalid decimal expected")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
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
pub async fn fetch_mintbay_status(rpc: &RpcClient, contract: &Address) -> Result<MintBayStatus> {
    let from = Address::ZERO;
    let data = Bytes::from(hex::decode("941ada0e").context("sel")?);
    let raw = rpc
        .eth_call(&from, contract, &data)
        .await
        .context("getMintStatus eth_call")?;
    if raw.len() < 17 * 32 {
        // Fallback: individual views
        return fetch_mintbay_status_fallback(rpc, contract).await;
    }
    Ok(MintBayStatus {
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
    })
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

// ─── Open detection ──────────────────────────────────────────────────────────

async fn eval_view_rule(
    rpc: &RpcClient,
    contract: &Address,
    rule: &ViewRule,
) -> Result<bool> {
    let data = build_calldata(&rule.function, &rule.params)?;
    let raw = rpc
        .eth_call(&Address::ZERO, contract, &data)
        .await
        .with_context(|| format!("view {}", rule.function))?;
    match rule.decode {
        DecodeKind::Uint256 => {
            let got = word_u256(&raw, 0)?;
            compare_u256(got, rule.op, &rule.expected)
        }
        DecodeKind::Bool => {
            let got = word_bool(&raw, 0)?;
            let exp = parse_u256_expected(&rule.expected)?;
            let exp_b = !exp.is_zero();
            Ok(match rule.op {
                CompareOp::Eq => got == exp_b,
                CompareOp::Ne => got != exp_b,
                _ => bail!("bool rules only support == / !="),
            })
        }
    }
}

async fn rules_open(rpc: &RpcClient, contract: &Address, rules: &[ViewRule]) -> Result<bool> {
    if rules.is_empty() {
        return Ok(false);
    }
    for r in rules {
        if !eval_view_rule(rpc, contract, r).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Probe estimate with mint calldata + value (one wallet).
async fn sim_open_ok(
    rpc: &RpcClient,
    contract: &Address,
    from: &Address,
    value: U256,
    calldata: &Bytes,
) -> bool {
    rpc.estimate_gas(from, contract, value, calldata)
        .await
        .is_ok()
}

struct OpenCheck {
    open: bool,
    detail: String,
    /// Suggested value for Auto (MintBay) when known.
    auto_value: Option<U256>,
}

async fn check_open(
    rpc: &RpcClient,
    config: &RawSniperConfig,
    calldata: &Bytes,
    probe: Option<&Address>,
    current_value: U256,
) -> OpenCheck {
    let wall = now_unix();

    // MintBay preset
    if matches!(config.preset, SniperPreset::MintBayPublic) {
        match fetch_mintbay_status(rpc, &config.contract).await {
            Ok(st) => {
                let open = st.is_public_open(wall);
                let val = st.mint_value(config.quantity);
                let detail = format!(
                    "MintBay phaseType={} phaseId={} paused={} minted={}/{} value={} wei",
                    st.current_phase_type,
                    st.resolved_phase_id,
                    st.minting_paused,
                    st.total_minted,
                    st.max_supply,
                    val
                );
                // Optional sim-open OR
                if !open && config.sim_open {
                    if let Some(from) = probe {
                        if sim_open_ok(rpc, &config.contract, from, val, calldata).await {
                            return OpenCheck {
                                open: true,
                                detail: format!("{detail} | sim-open OK"),
                                auto_value: Some(val),
                            };
                        }
                    }
                }
                return OpenCheck {
                    open,
                    detail,
                    auto_value: Some(val),
                };
            }
            Err(e) => {
                return OpenCheck {
                    open: false,
                    detail: format!("MintBay status err: {e}"),
                    auto_value: None,
                };
            }
        }
    }

    // Custom / Simple: rules AND
    let mut open = false;
    let mut detail = if !config.rules.is_empty() {
        match rules_open(rpc, &config.contract, &config.rules).await {
            Ok(ok) => {
                open = ok;
                format!("rules={}", if ok { "PASS" } else { "WAIT" })
            }
            Err(e) => format!("rules err: {e}"),
        }
    } else {
        "no rules".into()
    };

    if !open && config.sim_open {
        if let Some(from) = probe {
            if sim_open_ok(rpc, &config.contract, from, current_value, calldata).await {
                open = true;
                detail = format!("{detail} | sim-open OK");
            } else {
                detail = format!("{detail} | sim-open fail");
            }
        }
    }

    // Simple with only at_time: open when wall >= at_time (if no rules/sim)
    if !open
        && matches!(config.preset, SniperPreset::SimpleMintUint)
        && config.rules.is_empty()
        && !config.sim_open
    {
        if let Some(at) = config.at_time {
            if wall >= at {
                open = true;
                detail = format!("{detail} | at_time reached");
            } else {
                detail = format!("{detail} | wait at_time ({})", at - wall);
            }
        }
    }

    OpenCheck {
        open,
        detail,
        auto_value: None,
    }
}

fn resolve_value(config: &RawSniperConfig, auto: Option<U256>) -> U256 {
    match config.value_mode {
        ValueMode::Fixed => config.fixed_value,
        ValueMode::Auto => auto.unwrap_or(config.fixed_value),
    }
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

// ─── Main entry ──────────────────────────────────────────────────────────────

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
        Err(e) => {
            return fail_all(signers, format!("params: {e}"));
        }
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
    let (max_fee, max_priority_fee) =
        match gas::calculate_fees(&config.gas, base_fee, network_priority) {
            Ok(f) => f,
            Err(e) => return fail_all(signers, format!("gas: {e}")),
        };

    let probe = signers.first().map(|s| s.address());
    let start_wall = now_unix();
    let deadline = match config.at_time {
        Some(at) => at.saturating_add(config.timeout_secs as i64),
        None => start_wall.saturating_add(config.timeout_secs as i64),
    };

    report(
        &reporter,
        MintEvent::phase(
            "wait",
            format!(
                "Raw sniper: contract={:?} preset={:?} qty={} deadline_in={}s",
                config.contract,
                config.preset,
                config.quantity,
                (deadline - start_wall).max(0)
            ),
        ),
    );
    report(
        &reporter,
        MintEvent::message(format!(
            "Waiting for open | at_time={:?} mint_before_at={} sim_open={} dry_run={}",
            config.at_time, config.mint_before_at_time, config.sim_open, config.dry_run
        )),
    );

    // ── Coordinator wait loop ──
    let mut last_log = 0i64;
    let mut resolved_value = config.fixed_value;

    loop {
        if cancelled(&cancel) {
            report(
                &reporter,
                MintEvent::phase("error", "Cancelled while waiting"),
            );
            return fail_all(signers, "cancelled by user");
        }
        let wall = now_unix();
        if wall > deadline {
            report(
                &reporter,
                MintEvent::phase("error", "Timeout waiting for mint open"),
            );
            return fail_all(
                signers,
                format!("timeout: mint not open within {}s", config.timeout_secs),
            );
        }

        let value_guess = resolve_value(config, Some(resolved_value));
        let chk = check_open(rpc, config, &calldata, probe.as_ref(), value_guess).await;
        if let Some(v) = chk.auto_value {
            resolved_value = v;
        }

        let allow_fire = if chk.open {
            if config.mint_before_at_time {
                true
            } else if let Some(at) = config.at_time {
                wall >= at
            } else {
                true
            }
        } else {
            false
        };

        if wall - last_log >= 2 || allow_fire {
            last_log = wall;
            let left = (deadline - wall).max(0);
            report(
                &reporter,
                MintEvent::message(format!(
                    "poll: open={} fire={} | {} | deadline {}s | value {} wei",
                    chk.open,
                    allow_fire,
                    chk.detail,
                    left,
                    resolve_value(config, Some(resolved_value))
                )),
            );
        }

        if allow_fire {
            report(
                &reporter,
                MintEvent::phase(
                    "fire",
                    format!(
                        "OPEN — minting {} wallet(s), value {} wei",
                        signers.len(),
                        resolve_value(config, Some(resolved_value))
                    ),
                ),
            );
            break;
        }

        // Sleep: fast within 2s of at_time (or always if no at_time / past at_time)
        let fast = match config.at_time {
            Some(at) => wall >= at.saturating_sub(2),
            None => true,
        };
        let ms = if fast { 250 } else { 1000 };
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    if cancelled(&cancel) {
        return fail_all(signers, "cancelled by user");
    }

    let value = resolve_value(config, Some(resolved_value));
    // MintBay guard: Auto with fee-only free mint is fine; warn if auto zero and fixed also zero
    if matches!(config.preset, SniperPreset::MintBayPublic)
        && matches!(config.value_mode, ValueMode::Auto)
        && value.is_zero()
    {
        // free mint with zero fee is valid; continue
    }

    report(
        &reporter,
        MintEvent::message(format!(
            "Fan-out: {} wallets, concurrency={}, value={} wei, dry_run={}",
            signers.len(),
            config.concurrency.max(1),
            value,
            config.dry_run
        )),
    );

    // ── Parallel estimate → send ──
    let sem = Arc::new(Semaphore::new(config.concurrency.max(1)));
    let cancel_flag = cancel.clone();
    let mut handles = Vec::new();

    for signer in signers.iter().cloned() {
        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let rpc = rpc.clone();
        let calldata = calldata.clone();
        let contract = config.contract;
        let dry_run = config.dry_run;
        let rep = reporter.clone();
        let cancel_w = cancel_flag.clone();
        let deadline_w = deadline;
        let max_fee = max_fee;
        let max_priority_fee = max_priority_fee;
        let gas_multiplier = config.gas.gas_multiplier;
        let mut value = value;
        let value_mode = config.value_mode;
        let preset = config.preset;
        let quantity = config.quantity;
        let fixed_value = config.fixed_value;

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let addr = signer.address();
            let fail = |e: String| MintResult {
                address: addr,
                tx_hash: None,
                status: WalletStatus::Failed,
                gas_used: None,
                block_number: None,
                error: Some(e),
            };

            if cancelled(&cancel_w) {
                return fail("cancelled by user".into());
            }

            report(
                &rep,
                MintEvent::wallet(addr, Some(WalletStatus::Wait), Some("est".into()), None, None),
            );

            // Refresh auto value near fire (MintBay)
            if matches!(value_mode, ValueMode::Auto) && matches!(preset, SniperPreset::MintBayPublic)
            {
                if let Ok(st) = fetch_mintbay_status(&rpc, &contract).await {
                    value = st.mint_value(quantity);
                }
            } else if matches!(value_mode, ValueMode::Fixed) {
                value = fixed_value;
            }

            // Estimate retry until deadline
            let gas_estimate = loop {
                if cancelled(&cancel_w) {
                    return fail("cancelled by user".into());
                }
                if now_unix() > deadline_w {
                    return fail("timeout during estimate".into());
                }
                match rpc
                    .estimate_gas(&addr, &contract, value, &calldata)
                    .await
                {
                    Ok(g) => break g,
                    Err(e) => {
                        report(
                            &rep,
                            MintEvent::wallet(
                                addr,
                                Some(WalletStatus::Sim),
                                Some(format!("est fail: {e}")),
                                None,
                                None,
                            ),
                        );
                        // Refresh MintBay value on fail (price may have just set)
                        if matches!(value_mode, ValueMode::Auto)
                            && matches!(preset, SniperPreset::MintBayPublic)
                        {
                            if let Ok(st) = fetch_mintbay_status(&rpc, &contract).await {
                                value = st.mint_value(quantity);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            };

            report(
                &rep,
                MintEvent::wallet(
                    addr,
                    Some(WalletStatus::Sim),
                    Some(format!("est OK gas={gas_estimate} value={value}")),
                    None,
                    None,
                ),
            );

            let gas_limit = apply_gas_limit(gas_estimate, gas_multiplier, chain_id, 21_000);

            if dry_run {
                report(
                    &rep,
                    MintEvent::wallet(
                        addr,
                        Some(WalletStatus::DryRunOk),
                        Some("dry-run OK".into()),
                        None,
                        None,
                    ),
                );
                return MintResult {
                    address: addr,
                    tx_hash: None,
                    status: WalletStatus::DryRunOk,
                    gas_used: Some(gas_estimate),
                    block_number: None,
                    error: None,
                };
            }

            let nonce = match rpc.nonce(&addr).await {
                Ok(n) => n,
                Err(e) => return fail(format!("nonce: {e}")),
            };

            let tx = BuiltTx {
                chain_id,
                nonce,
                to: contract,
                value,
                data: calldata.clone(),
                gas_limit,
                max_fee,
                max_priority_fee,
            };

            let (raw, signed_hash) = match sign_transaction(&signer, &tx) {
                Ok(x) => x,
                Err(e) => return fail(format!("sign: {e}")),
            };

            report(
                &rep,
                MintEvent::wallet(
                    addr,
                    Some(WalletStatus::Sent),
                    Some(format!("sending {}", shorten_hash(&signed_hash))),
                    Some(signed_hash),
                    None,
                ),
            );

            let tx_hash = match rpc.race_send(&raw).await {
                Ok(h) => h,
                Err(e) => return fail(format!("send: {e}")),
            };

            match rpc.wait_for_receipt(&tx_hash, 120).await {
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
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        match h.await {
            Ok(r) => results.push(r),
            Err(e) => {
                crate::rlog!("wallet task join error: {e}");
            }
        }
    }

    report(
        &reporter,
        MintEvent::phase(
            "done",
            format!(
                "Raw sniper done: {}/{} ok",
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
        Some(s) => parse_at_time_unix(s)
            .map_err(|e| anyhow::anyhow!(e))
            .map(|o| o),
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

    #[test]
    fn compare_ops() {
        assert!(compare_u256(U256::from(2), CompareOp::Eq, "2").unwrap());
        assert!(compare_u256(U256::from(2), CompareOp::Gt, "1").unwrap());
        assert!(!compare_u256(U256::from(2), CompareOp::Lt, "1").unwrap());
    }
}
