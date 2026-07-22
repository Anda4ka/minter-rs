use alloy_primitives::{Address, Bytes, U256};
use anyhow::{Context, Result, bail};

use crate::rpc::RpcClient;
use crate::types::{GasMode, GasParams};

pub fn calculate_fees(
    params: &GasParams,
    base_fee: U256,
    network_priority: U256,
) -> Result<(U256, U256)> {
    match params.mode {
        GasMode::Manual => {
            let mf = params.max_fee.context("manual mode requires max_fee")?;
            let pf = params
                .priority_fee
                .context("manual mode requires priority_fee")?;
            if mf < pf {
                bail!("max fee cannot be lower than priority fee");
            }
            Ok((mf, pf))
        }
        GasMode::Hybrid => {
            let pf = params
                .priority_fee
                .context("hybrid mode requires priority_fee")?;
            let mf = params.max_fee.unwrap_or_else(|| {
                base_fee * U256::from((params.base_fee_multiplier * 100.0) as u64) / U256::from(100)
                    + pf
            });
            if mf < pf {
                bail!("max fee cannot be lower than priority fee");
            }
            Ok((mf, pf))
        }
        GasMode::Auto => {
            let pf = params.priority_fee.unwrap_or(network_priority);
            let mf = params.max_fee.unwrap_or_else(|| {
                base_fee * U256::from((params.base_fee_multiplier * 100.0) as u64) / U256::from(100)
                    + pf
            });
            if mf < pf {
                bail!("max fee cannot be lower than priority fee");
            }
            Ok((mf, pf))
        }
    }
}

/// L2 / Orbit chains where a bare 21k transfer often fails with "intrinsic gas too low".
pub fn chain_needs_elevated_gas(chain_id: u64) -> bool {
    matches!(
        chain_id,
        10 | // Optimism
        8453 | // Base
        42161 | // Arbitrum One
        42170 | // Arbitrum Nova
        81457 | // Blast
        7777777 | // Zora
        33139 | // ApeChain
        360 | // Shape
        4326 | // MegaETH
        4663 | // Robinhood Chain
        143 // Monad
    )
}

/// Apply multiplier + floors for a raw estimate.
pub fn apply_gas_limit(
    estimated: u64,
    gas_multiplier: f64,
    chain_id: u64,
    min_floor: u64,
) -> u64 {
    const MIN_EOA: u64 = 21_000;
    const L2_FLOOR: u64 = 150_000;
    const CAP: u64 = 15_000_000;

    let mult = if gas_multiplier > 1.0 {
        gas_multiplier
    } else {
        1.15
    };
    let base = estimated.max(MIN_EOA).max(min_floor);
    let mut limit = ((base as f64) * mult).ceil() as u64;
    limit = limit.max(base);
    if chain_needs_elevated_gas(chain_id) {
        limit = limit.max(L2_FLOOR);
    }
    limit.min(CAP)
}

/// eth_estimateGas for a call (any data) + L2-safe floor.
pub async fn resolve_call_gas(
    rpc: &RpcClient,
    from: &Address,
    to: &Address,
    value: U256,
    data: &Bytes,
    chain_id: u64,
    gas_multiplier: f64,
) -> u64 {
    const FALLBACK: u64 = 250_000;
    let estimated = match rpc.estimate_gas(from, to, value, data).await {
        Ok(g) => {
            crate::rlog!("  eth_estimateGas = {}", g);
            g
        }
        Err(e) => {
            crate::rlog!("  eth_estimateGas failed ({e}) — fallback {}", FALLBACK);
            FALLBACK
        }
    };
    apply_gas_limit(estimated, gas_multiplier, chain_id, 21_000)
}

/// Empty-data native transfer gas (Disperse / Sweep ETH).
pub async fn resolve_native_transfer_gas(
    rpc: &RpcClient,
    from: &Address,
    to: &Address,
    value: U256,
    chain_id: u64,
    gas_multiplier: f64,
) -> u64 {
    resolve_call_gas(
        rpc,
        from,
        to,
        value,
        &Bytes::new(),
        chain_id,
        gas_multiplier,
    )
    .await
}

/// Bump gas after "intrinsic gas too low" (capped).
pub fn bump_gas_after_intrinsic(current: u64) -> u64 {
    current.saturating_mul(3).max(300_000).min(2_000_000)
}

/// Intrinsic-gas-too-low detection. Kept here as a re-export for existing
/// callers; the implementation lives in [`crate::errors`] alongside the other
/// RPC/tx error classifiers.
pub use crate::errors::is_intrinsic_gas_too_low as is_intrinsic_gas_error;

/// Bump a fee by basis points without f64 (e.g. 11500 = ×1.15, 13000 = ×1.30).
/// If the result would not increase a non-zero fee, add 1 wei so RBF can progress.
pub fn bump_fee_bps(fee: U256, bps: u64) -> U256 {
    if bps == 0 {
        return U256::ZERO;
    }
    let bumped = fee.saturating_mul(U256::from(bps)) / U256::from(10_000u64);
    if !fee.is_zero() && bumped <= fee {
        fee.saturating_add(U256::from(1u64))
    } else {
        bumped
    }
}

/// Parse gwei decimal string → wei via integer math (9 fractional digits).
pub fn gwei_str_to_wei(s: &str) -> anyhow::Result<U256> {
    crate::amount::decimal_to_units(s.trim(), 9)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::max_retries_from_env;
    use std::collections::HashMap;

    #[test]
    fn auto_mode() {
        let params = GasParams::default();
        let base_fee = U256::from(20_000_000_000u64);
        let priority = U256::from(2_000_000_000u64);
        let (mf, pf) = calculate_fees(&params, base_fee, priority).unwrap();
        assert_eq!(pf, priority);
        assert!(mf >= base_fee * U256::from(2u64));
    }

    #[test]
    fn manual_mode_ok() {
        let params = GasParams {
            mode: GasMode::Manual,
            max_fee: Some(U256::from(100u64)),
            priority_fee: Some(U256::from(50u64)),
            ..GasParams::default()
        };
        let (mf, pf) = calculate_fees(&params, U256::ZERO, U256::ZERO).unwrap();
        assert_eq!(mf, U256::from(100u64));
        assert_eq!(pf, U256::from(50u64));
    }

    #[test]
    fn manual_mode_fee_below_priority() {
        let params = GasParams {
            mode: GasMode::Manual,
            max_fee: Some(U256::from(10u64)),
            priority_fee: Some(U256::from(50u64)),
            ..GasParams::default()
        };
        assert!(calculate_fees(&params, U256::ZERO, U256::ZERO).is_err());
    }

    #[test]
    fn hybrid_mode() {
        let params = GasParams {
            mode: GasMode::Hybrid,
            priority_fee: Some(U256::from(3_000_000_000u64)),
            ..GasParams::default()
        };
        let base_fee = U256::from(10_000_000_000u64);
        let (mf, pf) = calculate_fees(&params, base_fee, U256::ZERO).unwrap();
        assert_eq!(pf, U256::from(3_000_000_000u64));
        assert!(mf > pf);
    }

    #[test]
    fn from_env_reads_settings_keys() {
        let mut env = HashMap::new();
        env.insert("PRIORITY_FEE_GWEI".into(), "3".into());
        env.insert("BASE_FEE_MULTIPLIER".into(), "1.5".into());
        env.insert("GAS_MULTIPLIER".into(), "1.3".into());
        let params = GasParams::from_env(&env);
        assert_eq!(params.mode, GasMode::Hybrid);
        assert_eq!(params.priority_fee, Some(U256::from(3_000_000_000u64)));
        assert!((params.base_fee_multiplier - 1.5).abs() < 1e-9);
        assert!((params.gas_multiplier - 1.3).abs() < 1e-9);

        let (mf, pf) = calculate_fees(
            &params,
            U256::from(10_000_000_000u64),
            U256::from(1_000_000_000u64),
        )
        .unwrap();
        assert_eq!(pf, U256::from(3_000_000_000u64));
        // max_fee = base * 1.5 + priority = 15e9 + 3e9
        assert_eq!(mf, U256::from(18_000_000_000u64));
    }

    #[test]
    fn from_env_auto_priority() {
        let mut env = HashMap::new();
        env.insert("PRIORITY_FEE_GWEI".into(), "auto".into());
        let params = GasParams::from_env(&env);
        assert_eq!(params.mode, GasMode::Auto);
        assert!(params.priority_fee.is_none());
    }

    #[test]
    fn max_retries_from_env_values() {
        let mut env = HashMap::new();
        assert_eq!(max_retries_from_env(&env), 20);
        env.insert("MAX_RETRIES".into(), "5".into());
        assert_eq!(max_retries_from_env(&env), 5);
        env.insert("MAX_RETRIES".into(), "0".into());
        assert_eq!(max_retries_from_env(&env), 20);
    }

    #[test]
    fn l2_floor_robinhood() {
        assert!(chain_needs_elevated_gas(4663));
        let lim = apply_gas_limit(21_000, 1.15, 4663, 21_000);
        assert!(lim >= 150_000);
    }

    #[test]
    fn eth_mainnet_keeps_estimate_scale() {
        assert!(!chain_needs_elevated_gas(1));
        let lim = apply_gas_limit(21_000, 1.15, 1, 21_000);
        assert!(lim >= 21_000);
        assert!(lim < 150_000);
    }

    #[test]
    fn intrinsic_bump() {
        assert!(is_intrinsic_gas_error("intrinsic gas too low"));
        assert_eq!(bump_gas_after_intrinsic(21_000), 300_000);
    }

    #[test]
    fn fee_bps_bump_exact() {
        let base = U256::from(1_000_000_000u64); // 1 gwei
        assert_eq!(bump_fee_bps(base, 11_500), U256::from(1_150_000_000u64));
        assert_eq!(bump_fee_bps(base, 13_000), U256::from(1_300_000_000u64));
    }

    #[test]
    fn fee_bps_tiny_still_increases() {
        let fee = U256::from(1u64);
        // 1 * 11500 / 10000 = 1 (integer div) → force +1 wei
        assert_eq!(bump_fee_bps(fee, 11_500), U256::from(2u64));
    }

    #[test]
    fn gwei_str_to_wei_exact() {
        assert_eq!(
            gwei_str_to_wei("3").unwrap(),
            U256::from(3_000_000_000u64)
        );
        assert_eq!(
            gwei_str_to_wei("0.1").unwrap(),
            U256::from(100_000_000u64)
        );
        assert_eq!(
            gwei_str_to_wei("1.5").unwrap(),
            U256::from(1_500_000_000u64)
        );
    }
}
