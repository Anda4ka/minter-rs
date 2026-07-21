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
pub mod export;
pub mod gas;
pub mod mint;
pub mod mint_ops;
pub mod opensea;
pub mod progress;
pub mod proxy;
pub mod disperse;
pub mod flashbots;
pub mod multicall;
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

/// Truncate `s` to at most `max_bytes` **without splitting a UTF-8 character**.
///
/// Multi-byte-safe replacement for `&s[..s.len().min(n)]`, which panics when
/// byte `n` lands inside a multi-byte char (non-ASCII RPC / OpenSea / relay
/// error bodies would crash error formatting — the worst possible moment).
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

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

/// `println!` that respects `QUIET=1` (desktop default).
#[macro_export]
macro_rules! rlog {
    ($($arg:tt)*) => {{
        if !$crate::quiet_mode() {
            println!($($arg)*);
        }
    }};
}
pub use mint::{run_opensea_mint, MintRunSummary};
pub use mint_ops::{
    chain_mismatch_message, expand_wallet_quantities, explorer_tx_url, flashbots_allowed_for_chain,
    flashbots_status_label, is_on_chain_confirm_status, mint_busy_message, normalize_addr_key,
    parse_at_time_unix, reauth_required_message,
};
pub use raw_sniper::{RawSniperConfig, SniperPreset, ValueMode};
pub use progress::{MintEvent, MintReporter, NullReporter};
pub use safety_policy::{
    auth_concurrency_after_rate_limit, default_auth_concurrency, is_rate_limit_error,
    live_confirm_required, live_confirm_word_ok, no_proxy_multi_wallet_message,
    rate_limit_actionable_message, should_refresh_fees_at_fire, should_warn_no_proxy,
    FeeRefreshMode, MULTI_WALLET_PROXY_WARN_THRESHOLD,
};
pub use settings::Settings;
pub use types::*;
pub use vault::Vault;

/// Trust / product copy shared by all UIs.
pub const BURNER_WARNING: &str =
    "Use burner wallets only. Never import wallets with long-term funds.";
pub const NO_TELEMETRY: &str = "No telemetry. Private keys stay on this machine.";

#[cfg(test)]
mod truncate_tests {
    use super::truncate_str;

    #[test]
    fn ascii_truncates_exact() {
        assert_eq!(truncate_str("hello world", 5), "hello");
        assert_eq!(truncate_str("hi", 5), "hi");
        assert_eq!(truncate_str("", 5), "");
    }

    #[test]
    fn multibyte_boundary_does_not_panic() {
        // "яя…" — Cyrillic is 2 bytes per char; cutting at odd byte must back off.
        let s = "ошибка сети: тайм-аут";
        for n in 0..=s.len() + 2 {
            let t = truncate_str(s, n);
            assert!(t.len() <= n || s.len() <= n);
            assert!(s.starts_with(t));
        }
        // Emoji (4 bytes) mid-cut
        let e = "err 🔥🔥";
        assert_eq!(truncate_str(e, 5), "err ");
        assert_eq!(truncate_str(e, 8), "err 🔥");
    }
}
