use alloy_primitives::U256;
use anyhow::{Context, Result, bail};

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
}
