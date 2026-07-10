//! Shared business logic for MINTER desktop (and optional tools).
//!
//! No TUI/CLI UI dependencies.

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
pub mod raw_mint;
pub mod rpc;
pub mod settings;
pub mod sign;
pub mod sweep;
pub mod types;
pub mod vault;

pub use api::{
    AuthTestRow, DiscoveredFunction, DropPhasesResult, EligibilityResult, LatencyReport,
    LatencyRpcRow, MintOptions, ProxyHealthRow, ProxyListItem, RpcProbeResult, SecurityStatus,
    Session, StageRow, SweepResultRow, WalletBalanceRow, WalletEligibilityReport,
    WalletEligibilityRow, WalletInfo,
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
    chain_mismatch_message, expand_wallet_quantities, explorer_tx_url, is_on_chain_confirm_status,
    mint_busy_message, normalize_addr_key, parse_at_time_unix, reauth_required_message,
};
pub use progress::{MintEvent, MintReporter, NullReporter};
pub use settings::Settings;
pub use types::*;
pub use vault::Vault;

/// Trust / product copy shared by all UIs.
pub const BURNER_WARNING: &str =
    "Use burner wallets only. Never import wallets with long-term funds.";
pub const NO_TELEMETRY: &str = "No telemetry. Private keys stay on this machine.";
