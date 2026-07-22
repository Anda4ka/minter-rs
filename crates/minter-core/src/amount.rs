use alloy_primitives::U256;
use anyhow::{Context, Result, bail};

/// Convert a decimal amount string (e.g. `"0.08"`, `"1.5"`) into integer units
/// with the given number of fractional digits (18 for ETH/wei).
///
/// Does **not** use `f64` — only string/integer arithmetic — so values like
/// `0.08` convert exactly to `80000000000000000` wei.
///
/// Extra fractional digits beyond `decimals` are truncated (not rounded).
pub fn decimal_to_units(s: &str, decimals: u32) -> Result<U256> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty amount");
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    if s.starts_with('-') {
        bail!("negative amount not allowed");
    }
    if s.contains(['e', 'E']) {
        bail!("scientific notation not supported: {}", s);
    }

    let (int_raw, frac_raw) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };

    if int_raw.is_empty() && frac_raw.is_empty() {
        bail!("invalid amount");
    }

    let int_part = if int_raw.is_empty() { "0" } else { int_raw };
    if !int_part.chars().all(|c| c.is_ascii_digit()) {
        bail!("invalid integer part in amount: {}", s);
    }
    if !frac_raw.chars().all(|c| c.is_ascii_digit()) {
        bail!("invalid fractional part in amount: {}", s);
    }

    let decimals = decimals as usize;
    let mut frac = frac_raw.chars().take(decimals).collect::<String>();
    while frac.len() < decimals {
        frac.push('0');
    }

    let whole = U256::from_str_radix(int_part, 10)
        .with_context(|| format!("invalid integer amount: {}", int_part))?;
    let frac_u = if decimals == 0 || frac.chars().all(|c| c == '0') {
        U256::ZERO
    } else {
        U256::from_str_radix(&frac, 10)
            .with_context(|| format!("invalid fractional amount: {}", frac))?
    };

    let scale = U256::from(10u64).pow(U256::from(decimals as u64));
    whole
        .checked_mul(scale)
        .and_then(|v| v.checked_add(frac_u))
        .context("amount overflow")
}

/// ETH / native token amount string → wei (18 decimals).
pub fn eth_to_wei(s: &str) -> Result<U256> {
    decimal_to_units(s, 18)
}

/// Format wei as a short ETH decimal string for UI (display only).
pub fn wei_to_eth_string(wei: U256) -> String {
    // Avoid f64 for large values: whole + 6 fractional digits.
    let scale = U256::from(1_000_000_000_000_000_000u64); // 1e18
    let whole = wei / scale;
    let frac = wei % scale;
    // 6 decimals for display
    let micro_scale = U256::from(1_000_000_000_000u64); // 1e12 → 6 digits
    let frac6 = (frac / micro_scale).to::<u64>();
    format!("{}.{:06}", whole, frac6)
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// Extract a decimal amount string from a JSON value (string or number).
/// Prefers the original string form; for numbers uses `Number::to_string()`.
pub fn json_decimal_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Best-effort display float from a decimal string (UI only, not for value math).
pub fn decimal_string_to_f64(s: &str) -> Option<f64> {
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eth_zero() {
        assert_eq!(eth_to_wei("0").unwrap(), U256::ZERO);
        assert_eq!(eth_to_wei("0.0").unwrap(), U256::ZERO);
        assert_eq!(eth_to_wei(".0").unwrap(), U256::ZERO);
    }

    #[test]
    fn eth_one() {
        assert_eq!(
            eth_to_wei("1").unwrap(),
            U256::from(10u64).pow(U256::from(18u64))
        );
        assert_eq!(
            eth_to_wei("1.0").unwrap(),
            U256::from(10u64).pow(U256::from(18u64))
        );
    }

    #[test]
    fn eth_point_zero_eight() {
        // 0.08 ETH = 8e16 wei — classic f64-unsafe value
        assert_eq!(
            eth_to_wei("0.08").unwrap(),
            U256::from(80_000_000_000_000_000u64)
        );
    }

    #[test]
    fn eth_one_point_five() {
        assert_eq!(
            eth_to_wei("1.5").unwrap(),
            U256::from(1_500_000_000_000_000_000u64)
        );
    }

    #[test]
    fn eth_one_wei() {
        assert_eq!(
            eth_to_wei("0.000000000000000001").unwrap(),
            U256::from(1u64)
        );
    }

    #[test]
    fn truncates_extra_fractional_digits() {
        // 19th digit truncated
        assert_eq!(
            eth_to_wei("0.0000000000000000019").unwrap(),
            U256::from(1u64)
        );
    }

    #[test]
    fn rejects_negative_and_scientific() {
        assert!(eth_to_wei("-1").is_err());
        assert!(eth_to_wei("1e18").is_err());
        assert!(eth_to_wei("").is_err());
        assert!(eth_to_wei("abc").is_err());
    }

    #[test]
    fn json_string_and_number() {
        let s = serde_json::json!("0.08");
        assert_eq!(json_decimal_string(&s).as_deref(), Some("0.08"));
        let n = serde_json::json!(1);
        assert_eq!(json_decimal_string(&n).as_deref(), Some("1"));
    }

    #[test]
    fn custom_decimals() {
        // 6 decimals (USDC-style): 1.5 → 1_500_000
        assert_eq!(
            decimal_to_units("1.5", 6).unwrap(),
            U256::from(1_500_000u64)
        );
    }
}
