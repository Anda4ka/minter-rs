//! Shared business logic for MINTER desktop (and optional tools).
//!
//! No TUI/CLI UI dependencies.

// Clippy: large orchestration helpers intentionally carry many args (mint/api).
// Style noise (collapsible_if, etc.) deferred — see docs/RISK_MITIGATION_PLAN L1.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::useless_format)]
#![allow(clippy::manual_clamp)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::redundant_pattern_matching)]
#![allow(clippy::manual_repeat_n)]
#![allow(clippy::double_ended_iterator_last)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::needless_question_mark)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::clone_on_copy)]
#![allow(clippy::type_complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::redundant_locals)]
#![allow(clippy::unused_enumerate_index)]
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::match_like_matches_macro)]
#![allow(clippy::manual_map)]
#![allow(clippy::cloned_instead_of_copied)]

pub mod abi;
pub mod amount;
pub mod api;
pub mod auth_cache;
pub mod disperse;
pub mod errors;
pub mod export;
pub mod flashbots;
pub mod gas;
pub mod metrics;
pub mod mint;
pub mod mint_ops;
pub mod multicall;
pub mod opensea;
pub mod progress;
pub mod proxy;
pub mod raw_mint;
pub mod raw_sniper;
pub mod rpc;
pub mod safety_policy;
pub mod settings;
pub mod sign;
pub mod sweep;
pub mod types;
pub mod vault;

pub use api::{
    AuthTestRow, DiscoveredFunction, DropPhasesResult, EligibilityResult, LatencyReport,
    LatencyRpcRow, MintOptions, MulticallStepInput, NetworkProbeRow, ProxyHealthRow, ProxyListItem,
    RawProbeRow, RawSniperInput, RpcProbeResult, SecurityStatus, Session, StageRow, SweepResultRow,
    WalletBalanceRow, WalletEligibilityReport, WalletEligibilityRow, WalletInfo,
};

/// When true, suppress noisy `println!` in core (desktop sets QUIET=1 by default).
pub fn quiet_mode() -> bool {
    match std::env::var("QUIET") {
        Ok(v) => {
            let t = v.trim();
            t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

#[inline]
pub fn core_print(msg: impl AsRef<str>) {
    if !quiet_mode() {
        println!("{}", msg.as_ref());
    }
}

/// Byte-safe prefix of `s` up to `max_bytes`, never splitting a UTF-8 char.
///
/// `&s[..n]` panics when `n` lands inside a multi-byte code point (e.g. when
/// truncating a non-ASCII server error body), so all log/error truncation must
/// go through this instead of raw byte slicing.
#[inline]
pub fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Alias kept for older call sites / tests (`audit 2` name).
#[inline]
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    safe_truncate(s, max_bytes)
}

/// `println!` that respects `QUIET=1` (desktop default).
#[macro_export]
macro_rules! rlog {
    ($($arg:tt)*) => {{
        if !$crate::quiet_mode() {
            println!($($arg)*);
        }
    }};
}

/// `print!` (no newline, flushed) that respects `QUIET=1` (desktop default).
///
/// Raw `print!` bypassed the quiet gate, so partial "Simulating…"/"Sending…"
/// lines leaked to stdout even in the desktop's default `QUIET=1` mode.
#[macro_export]
macro_rules! rprint {
    ($($arg:tt)*) => {{
        if !$crate::quiet_mode() {
            print!($($arg)*);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }};
}
pub use mint::{MintRunSummary, run_opensea_mint};
pub use mint_ops::{
    chain_mismatch_message, expand_wallet_quantities, explorer_tx_url, flashbots_allowed_for_chain,
    flashbots_status_label, is_on_chain_confirm_status, mint_busy_message, normalize_addr_key,
    parse_at_time_unix, reauth_required_message,
};
pub use progress::{MintEvent, MintReporter, NullReporter};
pub use raw_sniper::{RawSniperConfig, SniperPreset, ValueMode};
pub use safety_policy::{
    FeeRefreshMode, MULTI_WALLET_PROXY_WARN_THRESHOLD, auth_concurrency_after_rate_limit,
    default_auth_concurrency, ensure_live_confirm, is_rate_limit_error,
    live_confirm_rejected_message, live_confirm_required, live_confirm_word_ok,
    no_proxy_multi_wallet_message, rate_limit_actionable_message, should_refresh_fees_at_fire,
    should_warn_no_proxy,
};
pub use settings::Settings;
pub use types::*;
pub use vault::Vault;

/// Trust / product copy shared by all UIs.
pub const BURNER_WARNING: &str =
    "Use burner wallets only. Never import wallets with long-term funds.";
pub const NO_TELEMETRY: &str = "No telemetry. Private keys stay on this machine.";

#[cfg(test)]
mod safe_truncate_tests {
    use super::{safe_truncate, truncate_str};

    #[test]
    fn shorter_than_limit_is_unchanged() {
        assert_eq!(safe_truncate("hello", 240), "hello");
        assert_eq!(safe_truncate("", 10), "");
    }

    #[test]
    fn ascii_truncates_exactly() {
        assert_eq!(safe_truncate("abcdef", 3), "abc");
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn never_splits_a_multibyte_char() {
        // "é" is 2 bytes (0xC3 0xA9). Truncating at byte 1 must back off to 0.
        let s = "é";
        assert_eq!(s.len(), 2);
        assert_eq!(safe_truncate(s, 1), "");
        assert_eq!(safe_truncate(s, 2), "é");
    }

    #[test]
    fn backs_off_to_char_boundary() {
        let s = "ошибка"; // 12 bytes, 6 chars
        let out = safe_truncate(s, 5); // 5 is mid-char → back to 4 bytes
        assert!(s.starts_with(out));
        assert!(out.len() <= 5);
        assert!(s.is_char_boundary(out.len()));
        // Emoji (4 bytes) mid-cut
        let e = "err 🔥🔥";
        assert_eq!(truncate_str(e, 5), "err ");
        assert_eq!(truncate_str(e, 8), "err 🔥");
    }
}
