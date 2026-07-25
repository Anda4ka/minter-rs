use alloy_primitives::{Address, Bytes, U256};
use anyhow::{Context, Result, bail};

use crate::rpc::RpcClient;
use crate::types::{GasMode, GasParams};

/// Ceiling for a tip adopted automatically from the network (50 gwei).
///
/// Only applies when the operator gave no explicit `priority_fee`. The network
/// value is a single block's 25th-percentile reward read from an untrusted RPC
/// and is paid verbatim, so it needs a sanity bound; an explicit user value is
/// never clamped.
pub const MAX_AUTO_PRIORITY_FEE_WEI: u64 = 50_000_000_000;

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
            // A zero max fee can never be mined, and it also makes the retry
            // loop non-terminating: the 4x ceiling is 0, and bumping 0 stays 0,
            // so the underpriced branch spins until attempts run out.
            if mf.is_zero() {
                bail!("max fee must be greater than zero");
            }
            Ok((mf, pf))
        }
        GasMode::Hybrid => {
            let pf = params
                .priority_fee
                .context("hybrid mode requires priority_fee")?;
            let mf = params.max_fee.unwrap_or_else(|| {
                mul_bps(base_fee, multiplier_to_bps(params.base_fee_multiplier)) + pf
            });
            if mf < pf {
                bail!("max fee cannot be lower than priority fee");
            }
            Ok((mf, pf))
        }
        GasMode::Auto => {
            // Bound the network-derived tip (see MAX_AUTO_PRIORITY_FEE_WEI).
            let pf = params
                .priority_fee
                .unwrap_or_else(|| network_priority.min(U256::from(MAX_AUTO_PRIORITY_FEE_WEI)));
            let mf = params.max_fee.unwrap_or_else(|| {
                mul_bps(base_fee, multiplier_to_bps(params.base_fee_multiplier)) + pf
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
pub fn apply_gas_limit(estimated: u64, gas_multiplier: f64, chain_id: u64, min_floor: u64) -> u64 {
    const MIN_EOA: u64 = 21_000;
    const L2_FLOOR: u64 = 150_000;
    const CAP: u64 = 15_000_000;

    // Integer bps instead of f64 so the limit is deterministic across platforms.
    let bps = if gas_multiplier > 1.0 {
        multiplier_to_bps(gas_multiplier)
    } else {
        11_500 // ×1.15 default headroom
    };
    let base = estimated.max(MIN_EOA).max(min_floor);
    let mut limit = ((base as u128 * bps as u128).div_ceil(10_000)) as u64;
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

/// OP-stack chains that charge a separate L1 data fee on top of L2 gas.
pub fn chain_has_l1_data_fee(chain_id: u64) -> bool {
    matches!(
        chain_id,
        10 |      // Optimism
        8453 |    // Base
        81457 |   // Blast
        7777777 | // Zora
        360 // Shape
    )
}

/// OP-stack `GasPriceOracle` predeploy address (same on every OP chain).
const OP_GAS_PRICE_ORACLE: Address = Address::new([
    0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0F,
]);

/// Conservative L1 data fee used when the oracle can't be read.
const L1_FEE_FALLBACK_WEI: u64 = 5_000_000_000_000;

/// Estimate the L1 data fee an OP-stack chain will charge for a simple native
/// transfer, in wei. Returns zero on non-OP chains.
///
/// On OP-stack chains the node checks
/// `balance >= value + gas_limit * max_fee_per_gas + l1_cost`, so a sweep that
/// reserves only `gas_limit * max_fee` and sends the rest is rejected for
/// insufficient funds by exactly this amount — every time.
pub async fn estimate_l1_data_fee(rpc: &RpcClient, chain_id: u64) -> U256 {
    if !chain_has_l1_data_fee(chain_id) {
        return U256::ZERO;
    }
    // getL1Fee(bytes) with a ~120-byte placeholder: the size of a signed EIP-1559
    // native transfer. Encoding: selector || offset(32) || len(120) || padded data.
    let mut data = crate::abi::function_selector("getL1Fee(bytes)").to_vec();
    data.extend_from_slice(&word_from_u64(32));
    data.extend_from_slice(&word_from_u64(120));
    data.extend_from_slice(&[0u8; 128]); // 120 bytes padded to a 32-byte multiple

    let raw = match rpc
        .eth_call(&Address::ZERO, &OP_GAS_PRICE_ORACLE, &Bytes::from(data))
        .await
    {
        Ok(b) if b.len() >= 32 => U256::from_be_slice(&b[..32]),
        _ => U256::from(L1_FEE_FALLBACK_WEI),
    };
    // 2x headroom: the L1 base fee can rise between this read and inclusion.
    raw.saturating_mul(U256::from(2u64))
        .max(U256::from(L1_FEE_FALLBACK_WEI))
}

fn word_from_u64(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
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
    // Must always make progress, including from zero: `0 * bps / 10000 == 0`
    // would otherwise leave an RBF/underpriced loop bumping forever with no
    // change and no ceiling to break it.
    if bumped <= fee {
        fee.saturating_add(U256::from(1u64))
    } else {
        bumped
    }
}

/// Convert an `f64` multiplier to basis points with explicit rounding, e.g.
/// `2.0 → 20_000`, `1.15 → 11_500`, `1.155 → 11_550`. Using bps (not integer
/// percent) keeps sub-percent multipliers meaningful, and `.round()` makes the
/// conversion deterministic instead of truncating like `(m * 100.0) as u64`.
pub fn multiplier_to_bps(m: f64) -> u64 {
    if !m.is_finite() || m <= 0.0 {
        return 0;
    }
    (m * 10_000.0).round() as u64
}

/// Multiply a `U256` by basis points (`value * bps / 10_000`) with no `f64` and
/// saturating multiplication. Deterministic across platforms.
pub fn mul_bps(value: U256, bps: u64) -> U256 {
    value.saturating_mul(U256::from(bps)) / U256::from(10_000u64)
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
        assert_eq!(gwei_str_to_wei("3").unwrap(), U256::from(3_000_000_000u64));
        assert_eq!(gwei_str_to_wei("0.1").unwrap(), U256::from(100_000_000u64));
        assert_eq!(
            gwei_str_to_wei("1.5").unwrap(),
            U256::from(1_500_000_000u64)
        );
    }

    #[test]
    fn multiplier_to_bps_rounds() {
        assert_eq!(multiplier_to_bps(2.0), 20_000);
        assert_eq!(multiplier_to_bps(1.15), 11_500);
        assert_eq!(multiplier_to_bps(1.155), 11_550); // sub-percent kept (was lost by *100 trunc)
        assert_eq!(multiplier_to_bps(0.0), 0);
        assert_eq!(multiplier_to_bps(-1.0), 0);
        assert_eq!(multiplier_to_bps(f64::NAN), 0);
    }

    #[test]
    fn mul_bps_exact() {
        let base = U256::from(1_000_000_000u64); // 1 gwei
        assert_eq!(mul_bps(base, 20_000), U256::from(2_000_000_000u64));
        assert_eq!(mul_bps(base, 11_500), U256::from(1_150_000_000u64));
        assert_eq!(mul_bps(U256::ZERO, 20_000), U256::ZERO);
    }

    #[test]
    fn calculate_fees_fractional_multiplier_is_precise() {
        // base_fee_multiplier 1.155 must apply ×1.155, not ×1.15 (old truncation).
        let params = GasParams {
            mode: GasMode::Auto,
            base_fee_multiplier: 1.155,
            priority_fee: Some(U256::ZERO),
            ..Default::default()
        };
        let base_fee = U256::from(1_000_000_000u64); // 1 gwei
        let (mf, _pf) = calculate_fees(&params, base_fee, U256::ZERO).unwrap();
        assert_eq!(mf, U256::from(1_155_000_000u64));
    }

    #[test]
    fn apply_gas_limit_matches_legacy_defaults() {
        // Default ×1.15 headroom on a plain estimate, integer path.
        assert_eq!(apply_gas_limit(100_000, 1.0, 1, 21_000), 115_000);
        assert_eq!(apply_gas_limit(100_000, 1.15, 1, 21_000), 115_000);
        // L2 floor applies (Base = 8453).
        assert!(apply_gas_limit(50_000, 1.15, 8453, 21_000) >= 150_000);
        // Never below the raw base.
        assert!(apply_gas_limit(200_000, 1.0, 1, 21_000) >= 200_000);
    }
}

#[cfg(test)]
mod auto_priority_cap_tests {
    use super::*;

    #[test]
    fn network_priority_is_capped_in_auto_mode() {
        let params = GasParams {
            mode: GasMode::Auto,
            base_fee_multiplier: 2.0,
            priority_fee: None, // adopt from network
            ..Default::default()
        };
        // Absurd tip from a hostile / gas-war RPC: 10_000 gwei.
        let insane = U256::from(10_000_000_000_000u64);
        let (mf, pf) = calculate_fees(&params, U256::from(1_000_000_000u64), insane).unwrap();
        assert_eq!(
            pf,
            U256::from(MAX_AUTO_PRIORITY_FEE_WEI),
            "tip must be clamped"
        );
        assert!(mf >= pf, "EIP-1559 invariant must hold");
    }

    #[test]
    fn sane_network_priority_passes_through() {
        let params = GasParams {
            mode: GasMode::Auto,
            base_fee_multiplier: 2.0,
            priority_fee: None,
            ..Default::default()
        };
        let normal = U256::from(1_500_000_000u64); // 1.5 gwei
        let (_mf, pf) = calculate_fees(&params, U256::from(1_000_000_000u64), normal).unwrap();
        assert_eq!(pf, normal);
    }

    #[test]
    fn explicit_priority_is_never_capped() {
        let huge = U256::from(500_000_000_000u64); // 500 gwei, user's explicit choice
        let params = GasParams {
            mode: GasMode::Auto,
            base_fee_multiplier: 2.0,
            priority_fee: Some(huge),
            ..Default::default()
        };
        let (_mf, pf) = calculate_fees(&params, U256::from(1_000_000_000u64), U256::ZERO).unwrap();
        assert_eq!(pf, huge, "explicit user value must not be clamped");
    }

    #[test]
    fn l1_data_fee_chains() {
        for id in [10u64, 8453, 81457, 7777777, 360] {
            assert!(chain_has_l1_data_fee(id), "chain {id} is OP-stack");
        }
        for id in [1u64, 137, 42161, 56] {
            assert!(!chain_has_l1_data_fee(id), "chain {id} is not OP-stack");
        }
    }
}
