//! Pure helpers for mint task options (qty map, schedule, explorer URLs).
//! Tested without network / Tauri.

use std::collections::HashMap;

/// Normalize address for map keys (`0x` + lowercase).
pub fn normalize_addr_key(addr: &str) -> String {
    let a = addr.trim().to_lowercase();
    if a.starts_with("0x") {
        a
    } else {
        format!("0x{a}")
    }
}

/// Parse task schedule string to unix seconds.
/// Accepts unix integer (seconds) or RFC3339 / ISO-8601.
/// Returns `None` for empty input; `Err` for invalid non-empty input.
pub fn parse_at_time_unix(raw: &str) -> Result<Option<i64>, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if let Ok(ts) = s.parse::<i64>() {
        if ts > 1_000_000_000_000 {
            // milliseconds
            return Ok(Some(ts / 1000));
        }
        return Ok(Some(ts));
    }
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(dt) => Ok(Some(dt.timestamp())),
        Err(e) => {
            // try without timezone as UTC
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                return Ok(Some(dt.and_utc().timestamp()));
            }
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
                return Ok(Some(dt.and_utc().timestamp()));
            }
            Err(format!("invalid at_time '{s}': {e}"))
        }
    }
}

/// Expand per-wallet quantities. Default qty applies when address missing.
/// Zero entries are dropped. Unknown addresses in map are kept (caller filters vault).
pub fn expand_wallet_quantities(
    default_qty: u32,
    wallet_addresses: &[String],
    per_wallet: Option<&HashMap<String, u32>>,
) -> HashMap<String, u32> {
    let default_qty = default_qty.max(1);
    let mut out = HashMap::new();
    for a in wallet_addresses {
        let k = normalize_addr_key(a);
        if k.len() < 10 {
            continue;
        }
        let q = per_wallet
            .and_then(|m| {
                m.get(&k)
                    .copied()
                    .or_else(|| m.get(a).copied())
                    .or_else(|| {
                        // try any key that normalizes equal
                        m.iter()
                            .find(|(kk, _)| normalize_addr_key(kk) == k)
                            .map(|(_, v)| *v)
                    })
            })
            .unwrap_or(default_qty);
        if q > 0 {
            out.insert(k, q);
        }
    }
    out
}

/// Block explorer tx URL for known chains (name or chain id string).
pub fn explorer_tx_url(chain: &str, tx_hash: &str) -> String {
    let h = tx_hash.trim();
    let h = if h.starts_with("0x") || h.starts_with("0X") {
        h.to_string()
    } else {
        format!("0x{h}")
    };
    let c = chain.trim().to_lowercase();
    let base = match c.as_str() {
        "ethereum" | "eth" | "mainnet" | "1" => "https://etherscan.io/tx/",
        "base" | "8453" => "https://basescan.org/tx/",
        "polygon" | "matic" | "137" => "https://polygonscan.com/tx/",
        "arbitrum" | "arb" | "42161" => "https://arbiscan.io/tx/",
        "optimism" | "op" | "10" => "https://optimistic.etherscan.io/tx/",
        "bsc" | "bnb" | "56" => "https://bscscan.com/tx/",
        "avalanche" | "avax" | "43114" => "https://snowtrace.io/tx/",
        "blast" | "81457" => "https://blastscan.io/tx/",
        "zora" | "7777777" => "https://explorer.zora.energy/tx/",
        "apechain" | "33139" => "https://apescan.io/tx/",
        _ => "https://etherscan.io/tx/",
    };
    format!("{base}{h}")
}

/// User-facing busy mint error (single-flight engine).
pub fn mint_busy_message() -> &'static str {
    "Mint already running — wait or Stop first"
}

/// User-facing chain mismatch hint.
pub fn chain_mismatch_message(expected: &str, got: &str) -> String {
    format!(
        "Chain mismatch: collection expects '{expected}', RPC reports '{got}'. Open Settings → Connection and set matching RPC / Alchemy."
    )
}

/// User-facing OpenSea re-auth hint.
pub fn reauth_required_message() -> &'static str {
    "OpenSea auth expired (401) — re-auth will retry; if it keeps failing check proxies / SIWE"
}

/// True if wallet status string is an on-chain confirm (not dry-run OK / sim).
/// Desktop emits first-confirm badge only for these, and only once per run.
pub fn is_on_chain_confirm_status(status: &str) -> bool {
    status.to_uppercase().contains("CONFIRM")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unix_seconds() {
        assert_eq!(parse_at_time_unix("1700000000").unwrap(), Some(1_700_000_000));
    }

    #[test]
    fn parse_unix_millis() {
        assert_eq!(
            parse_at_time_unix("1700000000000").unwrap(),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn parse_rfc3339() {
        let ts = parse_at_time_unix("2023-11-14T22:13:20Z").unwrap().unwrap();
        assert_eq!(ts, 1_700_000_000);
    }

    #[test]
    fn parse_empty_none() {
        assert_eq!(parse_at_time_unix("  ").unwrap(), None);
    }

    #[test]
    fn parse_invalid_err() {
        assert!(parse_at_time_unix("not-a-time").is_err());
    }

    #[test]
    fn qty_default_and_override() {
        let addrs = vec![
            "0xAAAAaaaaAAAAaaaaAAAAaaaaAAAAaaaaAAAAaaaa".into(),
            "0xBBBBbbbbBBBBbbbbBBBBbbbbBBBBbbbbBBBBbbbb".into(),
        ];
        let mut m = HashMap::new();
        m.insert(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            3u32,
        );
        let out = expand_wallet_quantities(1, &addrs, Some(&m));
        assert_eq!(
            out.get("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some(&3)
        );
        assert_eq!(
            out.get("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            Some(&1)
        );
    }

    #[test]
    fn qty_drops_zero() {
        let addrs = vec!["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()];
        let mut m = HashMap::new();
        m.insert(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            0u32,
        );
        let out = expand_wallet_quantities(2, &addrs, Some(&m));
        assert!(out.is_empty());
    }

    #[test]
    fn explorer_base_and_eth() {
        let u = explorer_tx_url("base", "abc");
        assert!(u.starts_with("https://basescan.org/tx/0x"));
        let u2 = explorer_tx_url("ethereum", "0xdead");
        assert!(u2.contains("etherscan.io"));
    }

    #[test]
    fn messages_nonempty() {
        assert!(!mint_busy_message().is_empty());
        assert!(chain_mismatch_message("base", "1").contains("Settings"));
        assert!(reauth_required_message().contains("401"));
    }

    #[test]
    fn on_chain_confirm_status_not_dry_ok() {
        assert!(is_on_chain_confirm_status("CONFIRMED"));
        assert!(is_on_chain_confirm_status("Confirmed"));
        assert!(!is_on_chain_confirm_status("OK"));
        assert!(!is_on_chain_confirm_status("DRY_RUN_OK"));
        assert!(!is_on_chain_confirm_status("SIM"));
        assert!(!is_on_chain_confirm_status("FAILED"));
    }

    #[test]
    fn first_confirm_gate_fires_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let gate = AtomicBool::new(false);
        let mut emits = 0;
        for st in ["CONFIRMED", "CONFIRMED", "CONFIRMED"] {
            if is_on_chain_confirm_status(st)
                && gate
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                emits += 1;
            }
        }
        assert_eq!(emits, 1);
    }
}
