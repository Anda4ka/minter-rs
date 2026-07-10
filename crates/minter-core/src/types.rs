use alloy_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

pub const VAULT_FILE: &str = "keys.vault";
pub const VAULT_SALT_LEN: usize = 32;
pub const VAULT_IV_LEN: usize = 12;
pub const VAULT_TAG_LEN: usize = 16;
pub const VAULT_KDF_ITERATIONS: u32 = 600_000;

pub type Signer = alloy::signers::local::LocalSigner<k256::ecdsa::SigningKey>;

pub fn chain_id_map() -> HashMap<&'static str, u64> {
    let mut m = HashMap::new();
    m.insert("ethereum", 1);
    m.insert("mainnet", 1);
    m.insert("matic", 137);
    m.insert("polygon", 137);
    m.insert("base", 8453);
    m.insert("arbitrum", 42161);
    m.insert("arbitrum_nova", 42170);
    m.insert("arbitrum-nova", 42170);
    m.insert("nova", 42170);
    m.insert("optimism", 10);
    m.insert("zora", 7777777);
    m.insert("avalanche", 43114);
    m.insert("bsc", 56);
    m.insert("blast", 81457);
    m.insert("shape", 360);
    m.insert("ape_chain", 33139);
    m.insert("apechain", 33139);
    m.insert("monad", 143);
    m.insert("megaeth", 4326);
    m.insert("mega_eth", 4326);
    m.insert("robinhood", 4663);
    m.insert("robinhood_chain", 4663);
    m.insert("robinhood-chain", 4663);
    m
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChainId {
    Ethereum = 1,
    Matic = 137,
    Base = 8453,
    Arbitrum = 42161,
    ArbitrumNova = 42170,
    Optimism = 10,
    Zora = 7777777,
    Avalanche = 43114,
    Bsc = 56,
    Blast = 81457,
    Shape = 360,
    ApeChain = 33139,
    Monad = 143,
    MegaEth = 4326,
    Robinhood = 4663,
}

impl ChainId {
    pub fn from_id(id: u64) -> Option<Self> {
        match id {
            1 => Some(Self::Ethereum),
            137 => Some(Self::Matic),
            8453 => Some(Self::Base),
            42161 => Some(Self::Arbitrum),
            42170 => Some(Self::ArbitrumNova),
            10 => Some(Self::Optimism),
            7777777 => Some(Self::Zora),
            43114 => Some(Self::Avalanche),
            56 => Some(Self::Bsc),
            81457 => Some(Self::Blast),
            360 => Some(Self::Shape),
            33139 => Some(Self::ApeChain),
            143 => Some(Self::Monad),
            4326 => Some(Self::MegaEth),
            4663 => Some(Self::Robinhood),
            _ => None,
        }
    }

    pub fn id(&self) -> u64 {
        *self as u64
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Ethereum => "ethereum",
            Self::Matic => "polygon",
            Self::Base => "base",
            Self::Arbitrum => "arbitrum",
            Self::ArbitrumNova => "arbitrum_nova",
            Self::Optimism => "optimism",
            Self::Zora => "zora",
            Self::Avalanche => "avalanche",
            Self::Bsc => "bsc",
            Self::Blast => "blast",
            Self::Shape => "shape",
            Self::ApeChain => "apechain",
            Self::Monad => "monad",
            Self::MegaEth => "megaeth",
            Self::Robinhood => "robinhood",
        }
    }

    pub fn all() -> Vec<ChainId> {
        vec![
            Self::Ethereum,
            Self::Base,
            Self::Matic,
            Self::Arbitrum,
            Self::ArbitrumNova,
            Self::Optimism,
            Self::Zora,
            Self::Avalanche,
            Self::Bsc,
            Self::Blast,
            Self::Shape,
            Self::ApeChain,
            Self::Monad,
            Self::MegaEth,
            Self::Robinhood,
        ]
    }
}

impl fmt::Display for ChainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (chainId {})", self.name(), self.id())
    }
}

#[cfg(test)]
mod chain_id_tests {
    use super::*;

    #[test]
    fn arbitrum_nova_roundtrip() {
        assert_eq!(ChainId::from_id(42170), Some(ChainId::ArbitrumNova));
        assert_eq!(ChainId::ArbitrumNova.id(), 42170);
        assert_eq!(ChainId::ArbitrumNova.name(), "arbitrum_nova");
        assert_eq!(chain_id_map().get("arbitrum_nova"), Some(&42170));
        assert_eq!(chain_id_map().get("nova"), Some(&42170));
        assert!(ChainId::all().contains(&ChainId::ArbitrumNova));
    }

    #[test]
    fn megaeth_and_monad() {
        assert_eq!(ChainId::from_id(4326), Some(ChainId::MegaEth));
        assert_eq!(ChainId::MegaEth.id(), 4326);
        assert_eq!(ChainId::MegaEth.name(), "megaeth");
        assert_eq!(chain_id_map().get("megaeth"), Some(&4326));
        assert_eq!(ChainId::from_id(143), Some(ChainId::Monad));
        assert_eq!(ChainId::Monad.id(), 143);
        assert_eq!(chain_id_map().get("monad"), Some(&143));
    }

    #[test]
    fn robinhood_chain() {
        assert_eq!(ChainId::from_id(4663), Some(ChainId::Robinhood));
        assert_eq!(ChainId::Robinhood.id(), 4663);
        assert_eq!(ChainId::Robinhood.name(), "robinhood");
        assert_eq!(chain_id_map().get("robinhood"), Some(&4663));
        assert_eq!(chain_id_map().get("robinhood_chain"), Some(&4663));
        assert!(ChainId::all().contains(&ChainId::Robinhood));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletStatus {
    Wait,
    Auth,
    Calldata,
    Sim,
    Sent,
    Confirmed,
    Failed,
    DryRunOk,
}

impl fmt::Display for WalletStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wait => write!(f, "WAIT"),
            Self::Auth => write!(f, "AUTH"),
            Self::Calldata => write!(f, "CALLDATA"),
            Self::Sim => write!(f, "SIM"),
            Self::Sent => write!(f, "SENT"),
            Self::Confirmed => write!(f, "CONFIRMED"),
            Self::Failed => write!(f, "FAILED"),
            Self::DryRunOk => write!(f, "DRY_RUN_OK"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MintResult {
    pub address: Address,
    pub tx_hash: Option<B256>,
    pub status: WalletStatus,
    pub gas_used: Option<u64>,
    pub block_number: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultEntry {
    pub address: String,
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GasMode {
    Auto,
    Hybrid,
    Manual,
}

#[derive(Debug, Clone)]
pub struct GasParams {
    pub mode: GasMode,
    pub max_fee: Option<U256>,
    pub priority_fee: Option<U256>,
    pub base_fee_multiplier: f64,
    pub gas_multiplier: f64,
}

impl Default for GasParams {
    fn default() -> Self {
        Self {
            mode: GasMode::Auto,
            max_fee: None,
            priority_fee: None,
            base_fee_multiplier: 2.0,
            gas_multiplier: 1.15,
        }
    }
}

impl GasParams {
    /// Build gas params from env/settings keys:
    /// `PRIORITY_FEE_GWEI`, `BASE_FEE_MULTIPLIER`, `GAS_MULTIPLIER`.
    pub fn from_env(env: &std::collections::HashMap<String, String>) -> Self {
        let mut params = Self::default();

        if let Some(v) = env
            .get("BASE_FEE_MULTIPLIER")
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            if v > 0.0 {
                params.base_fee_multiplier = v;
            }
        }

        if let Some(v) = env
            .get("GAS_MULTIPLIER")
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            if v > 0.0 {
                params.gas_multiplier = v;
            }
        }

        if let Some(v) = env.get("PRIORITY_FEE_GWEI") {
            let t = v.trim();
            if !t.is_empty() && !t.eq_ignore_ascii_case("auto") {
                if let Ok(pg) = t.parse::<f64>() {
                    if pg > 0.0 {
                        params.mode = GasMode::Hybrid;
                        // gwei → wei; cap cast at u64::MAX gwei equivalent is fine for fees
                        let wei = (pg * 1e9) as u128;
                        params.priority_fee = Some(U256::from(wei));
                    }
                }
            }
        }

        params
    }

    /// Override priority fee (gwei). Used for interactive prompt over env defaults.
    pub fn with_priority_gwei(mut self, gwei: f64) -> Self {
        if gwei > 0.0 {
            self.mode = GasMode::Hybrid;
            self.priority_fee = Some(U256::from((gwei * 1e9) as u128));
        }
        self
    }
}

/// Max mint attempts from `MAX_RETRIES` env (default 20, minimum 1).
pub fn max_retries_from_env(env: &std::collections::HashMap<String, String>) -> u32 {
    env.get("MAX_RETRIES")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(20)
}

/// Parse truthy env flags: 1/true/yes/on (case-insensitive).
pub fn env_flag(env: &std::collections::HashMap<String, String>, key: &str, default: bool) -> bool {
    match env.get(key) {
        None => default,
        Some(v) => {
            let t = v.trim();
            if t.is_empty() {
                return default;
            }
            t == "1"
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("on")
        }
    }
}

pub fn quiet_from_env(env: &std::collections::HashMap<String, String>) -> bool {
    env_flag(env, "QUIET", false)
}

pub fn skip_preflight_from_env(env: &std::collections::HashMap<String, String>) -> bool {
    env_flag(env, "SKIP_PREFLIGHT", false)
}

pub fn beep_from_env(env: &std::collections::HashMap<String, String>) -> bool {
    env_flag(env, "BEEP", true)
}

pub fn export_results_from_env(env: &std::collections::HashMap<String, String>) -> bool {
    env_flag(env, "EXPORT_RESULTS", true)
}

/// True if error string looks like OpenSea/HTTP auth failure.
pub fn is_auth_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("not authenticated")
        || lower.contains("authentication required")
        || lower.contains("invalid token")
        || (lower.contains("access token")
            && (lower.contains("expired") || lower.contains("invalid")))
}
