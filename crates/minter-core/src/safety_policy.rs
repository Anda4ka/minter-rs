//! Pure operator-safety policy helpers (unit-tested, no network I/O).
//!
//! Used by desktop UI gates and core mint/sniper paths.

use serde::{Deserialize, Serialize};

/// Multi-wallet OpenSea starts without proxies tend to hit 429.
pub const MULTI_WALLET_PROXY_WARN_THRESHOLD: usize = 4;

/// Seconds to keep auth serial after a rate-limit hit.
pub const AUTH_RATE_LIMIT_SERIAL_SECS: u64 = 15;

/// True when starting many wallets with zero proxies should warn the operator.
pub fn should_warn_no_proxy(wallet_count: usize, proxy_count: usize) -> bool {
    wallet_count >= MULTI_WALLET_PROXY_WARN_THRESHOLD && proxy_count == 0
}

/// Human-facing message for the multi-wallet / no-proxy gate.
pub fn no_proxy_multi_wallet_message(wallet_count: usize) -> String {
    format!(
        "OpenSea rate-limits a single IP. You selected {wallet_count} wallets with 0 proxies. \
         Add proxies in Settings/Proxies, or continue at your own risk (expect 429 / failed auth)."
    )
}

/// Detect OpenSea / HTTP rate-limit style errors.
pub fn is_rate_limit_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("too_many_requests")
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("503")
        || lower.contains("retry-after")
}

/// Actionable operator message when auth/GQL hits 429.
pub fn rate_limit_actionable_message() -> &'static str {
    "OpenSea rate limit (429/503). Add proxies, lower AUTH_CONCURRENCY (try 1), wait, then retry. \
     Without proxies multi-wallet auth is throttled to serial mode after a 429."
}

/// LIVE confirm gate: required when setting is on and run is not dry-run.
pub fn live_confirm_required(require_live_confirm: bool, dry_run: bool) -> bool {
    require_live_confirm && !dry_run
}

/// Accept typed LIVE confirmation (case-insensitive trim).
pub fn live_confirm_word_ok(word: &str) -> bool {
    word.trim().eq_ignore_ascii_case("LIVE")
}

/// When to re-fetch fees and re-sign raw sniper txs at fire time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FeeRefreshMode {
    /// Ethereum mainnet only (default — no extra latency on L2).
    #[default]
    MainnetOnly,
    /// All chains (opt-in; adds fee_history + possible re-sign at T0).
    Always,
    /// Never refresh at fire.
    Never,
}

impl FeeRefreshMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MainnetOnly => "mainnetOnly",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" | "all" | "1" | "true" | "yes" => Self::Always,
            "never" | "off" | "0" | "false" | "no" => Self::Never,
            _ => Self::MainnetOnly,
        }
    }
}

/// Whether raw sniper should call fee_history / re-sign at fire for this chain.
pub fn should_refresh_fees_at_fire(chain_id: u64, mode: FeeRefreshMode) -> bool {
    match mode {
        FeeRefreshMode::Never => false,
        FeeRefreshMode::Always => true,
        FeeRefreshMode::MainnetOnly => chain_id == 1,
    }
}

/// Default auth concurrency: low without proxies, modest with proxies.
pub fn default_auth_concurrency(proxy_count: usize) -> usize {
    if proxy_count == 0 {
        2
    } else {
        proxy_count.clamp(2, 6)
    }
}

/// After a rate-limit event, force serial auth (1 concurrent).
pub fn auth_concurrency_after_rate_limit() -> usize {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_warn_threshold() {
        assert!(!should_warn_no_proxy(3, 0));
        assert!(should_warn_no_proxy(4, 0));
        assert!(should_warn_no_proxy(10, 0));
        assert!(!should_warn_no_proxy(10, 1));
        assert!(!should_warn_no_proxy(0, 0));
    }

    #[test]
    fn live_confirm_gate() {
        assert!(live_confirm_required(true, false));
        assert!(!live_confirm_required(true, true));
        assert!(!live_confirm_required(false, false));
        assert!(!live_confirm_required(false, true));
        assert!(live_confirm_word_ok("LIVE"));
        assert!(live_confirm_word_ok(" live "));
        assert!(!live_confirm_word_ok("OK"));
    }

    #[test]
    fn fee_refresh_modes() {
        assert!(should_refresh_fees_at_fire(1, FeeRefreshMode::MainnetOnly));
        assert!(!should_refresh_fees_at_fire(
            8453,
            FeeRefreshMode::MainnetOnly
        ));
        assert!(should_refresh_fees_at_fire(8453, FeeRefreshMode::Always));
        assert!(!should_refresh_fees_at_fire(1, FeeRefreshMode::Never));
        assert_eq!(FeeRefreshMode::parse("always"), FeeRefreshMode::Always);
        assert_eq!(FeeRefreshMode::parse("never"), FeeRefreshMode::Never);
        assert_eq!(FeeRefreshMode::parse(""), FeeRefreshMode::MainnetOnly);
    }

    #[test]
    fn rate_limit_detect() {
        assert!(is_rate_limit_error("HTTP 429 Too Many Requests"));
        assert!(is_rate_limit_error("rate limit exceeded"));
        assert!(!is_rate_limit_error("execution reverted"));
        assert!(!rate_limit_actionable_message().is_empty());
    }

    #[test]
    fn auth_concurrency_defaults() {
        assert_eq!(default_auth_concurrency(0), 2);
        assert_eq!(default_auth_concurrency(1), 2);
        assert_eq!(default_auth_concurrency(10), 6);
        assert_eq!(auth_concurrency_after_rate_limit(), 1);
    }
}
