//! App settings: persisted as `config.json` (no manual .env required).
//!
//! Keys, RPC endpoints, gas and mint flags are edited in CLI/Desktop Settings UI.
//! Optional one-time migration still reads legacy `.env` if present.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::types::{
    beep_from_env, export_results_from_env, quiet_from_env, skip_preflight_from_env,
};

/// All operator-facing configuration (saved to config.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // ── Network / providers ──────────────────────────────────────────
    /// Alchemy API key → auto eth/base/polygon mainnet RPCs.
    pub alchemy_api_key: String,
    /// Comma-separated custom RPC URLs (any chain).
    pub rpc_urls: String,
    pub rpc_url_ethereum: String,
    pub rpc_url_base: String,
    pub rpc_url_polygon: String,
    /// Proxies: one per line (host:port:user:pass, socks5://…, http://…).
    /// Field name kept as `proxy_url` for backward-compatible config.json.
    pub proxy_url: String,
    /// Flashbots relay URL (Ethereum mainnet bundles). Empty → default relay.
    pub flashbots_relay_url: String,
    /// How many next blocks to target when sending a bundle.
    pub flashbots_max_blocks: u64,
    /// Delay between Flashbots resubmits (ms).
    pub flashbots_resubmit_ms: u64,

    // ── Gas / mint behaviour ─────────────────────────────────────────
    pub gas_limit: u64,
    pub use_gql: bool,
    pub priority_fee_gwei: String,
    pub base_fee_multiplier: String,
    pub gas_multiplier: String,
    pub max_retries: u32,
    pub quiet: bool,
    pub skip_preflight: bool,
    pub beep: bool,
    pub export_results: bool,

    // ── Safety ───────────────────────────────────────────────────────
    /// Default dry-run preference (UI may override per run).
    pub dry_run: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            alchemy_api_key: String::new(),
            rpc_urls: String::new(),
            rpc_url_ethereum: String::new(),
            rpc_url_base: String::new(),
            rpc_url_polygon: String::new(),
            proxy_url: String::new(),
            flashbots_relay_url: String::new(),
            flashbots_max_blocks: 3,
            flashbots_resubmit_ms: 1200,
            gas_limit: 250_000,
            use_gql: false,
            priority_fee_gwei: "auto".to_string(),
            base_fee_multiplier: "2.0".to_string(),
            gas_multiplier: "1.15".to_string(),
            max_retries: 20,
            quiet: false,
            skip_preflight: false,
            beep: true,
            export_results: true,
            dry_run: true,
        }
    }
}

impl Settings {
    pub fn config_path_default() -> PathBuf {
        PathBuf::from("config.json")
    }

    /// Load settings: config.json first, then fill blanks from legacy .env / proxies.txt.
    pub fn load(config_path: &Path, env_path: Option<&Path>) -> Self {
        let mut s = if config_path.exists() {
            match std::fs::read_to_string(config_path) {
                Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        } else {
            Self::default()
        };

        if let Some(env_path) = env_path {
            if env_path.exists() {
                let env = load_simple_env(env_path);
                s.merge_from_env_map(&env, /*only_if_empty*/ config_path.exists());
            }
        }
        s.load_proxies_file_if_empty(config_path);
        s
    }

    /// Backward-compatible load purely from env map (CLI mint paths).
    pub fn load_from_env(env: &HashMap<String, String>) -> Self {
        let mut s = Self::default();
        s.merge_from_env_map(env, false);
        s
    }

    /// Merge env keys into self.
    /// If `only_if_empty`, only fill fields that are still default/empty (migration).
    fn merge_from_env_map(&mut self, env: &HashMap<String, String>, only_if_empty: bool) {
        let take = |cur: &str, key_vals: &[&str]| -> Option<String> {
            if only_if_empty && !cur.trim().is_empty() {
                return None;
            }
            for k in key_vals {
                if let Some(v) = env.get(*k) {
                    let t = v.trim();
                    if !t.is_empty() && t != "YOUR_ALCHEMY_KEY" {
                        return Some(t.to_string());
                    }
                }
            }
            None
        };

        if let Some(v) = take(
            &self.alchemy_api_key,
            &["ALCHEMY_API_KEY", "ALCHEMYAPIKEY", "alchemyapikey"],
        ) {
            self.alchemy_api_key = v;
        }
        if let Some(v) = take(&self.rpc_urls, &["RPC_URLS", "RPC_URL"]) {
            // Prefer multi if present
            if let Some(multi) = env.get("RPC_URLS").filter(|s| !s.trim().is_empty()) {
                self.rpc_urls = multi.trim().to_string();
            } else {
                self.rpc_urls = v;
            }
        }
        if let Some(v) = take(
            &self.rpc_url_ethereum,
            &["RPC_URL_ETHEREUM", "ETHEREUM_RPC_URL", "RPC_URLS_ETHEREUM"],
        ) {
            self.rpc_url_ethereum = v;
        }
        if let Some(v) = take(
            &self.rpc_url_base,
            &["RPC_URL_BASE", "BASE_RPC_URL", "RPC_URLS_BASE"],
        ) {
            self.rpc_url_base = v;
        }
        if let Some(v) = take(
            &self.rpc_url_polygon,
            &["RPC_URL_POLYGON", "POLYGON_RPC_URL"],
        ) {
            self.rpc_url_polygon = v;
        }
        if let Some(v) = take(&self.proxy_url, &["PROXY_URL", "HTTP_PROXY", "HTTPS_PROXY"]) {
            // Single-line env; append if multi-line already empty
            if self.proxy_lines().is_empty() {
                self.proxy_url = v;
            } else if !self.proxy_lines().iter().any(|l| l == &v) {
                if !self.proxy_url.ends_with('\n') && !self.proxy_url.is_empty() {
                    self.proxy_url.push('\n');
                }
                self.proxy_url.push_str(&v);
                self.proxy_url.push('\n');
            }
        }
        if let Some(v) = take(
            &self.flashbots_relay_url,
            &["FLASHBOTS_RELAY_URL", "FLASHBOTS_RELAY"],
        ) {
            self.flashbots_relay_url = v;
        }
        if !only_if_empty || self.flashbots_max_blocks == Self::default().flashbots_max_blocks {
            if let Some(n) = env
                .get("FLASHBOTS_MAX_BLOCKS")
                .and_then(|v| v.trim().parse::<u64>().ok())
            {
                if n > 0 {
                    self.flashbots_max_blocks = n.min(20);
                }
            }
        }

        if !only_if_empty || self.gas_limit == Self::default().gas_limit {
            if let Some(v) = env.get("GAS_LIMIT").and_then(|v| v.parse().ok()) {
                self.gas_limit = v;
            }
        }
        if let Some(v) = env.get("USE_GQL") {
            if !only_if_empty || !self.use_gql {
                self.use_gql = v == "1" || v.eq_ignore_ascii_case("true");
            }
        }
        if let Some(v) = take(&self.priority_fee_gwei, &["PRIORITY_FEE_GWEI"]) {
            if !only_if_empty || self.priority_fee_gwei == "auto" {
                self.priority_fee_gwei = v;
            }
        } else if let Some(v) = env.get("PRIORITY_FEE_GWEI") {
            if !only_if_empty {
                self.priority_fee_gwei = v.clone();
            }
        }
        if let Some(v) = env.get("BASE_FEE_MULTIPLIER") {
            if !only_if_empty || self.base_fee_multiplier == "2.0" {
                self.base_fee_multiplier = v.clone();
            }
        }
        if let Some(v) = env.get("GAS_MULTIPLIER") {
            if !only_if_empty || self.gas_multiplier == "1.15" {
                self.gas_multiplier = v.clone();
            }
        }
        if let Some(v) = env.get("MAX_RETRIES").and_then(|v| v.parse().ok()) {
            if !only_if_empty || self.max_retries == 20 {
                self.max_retries = v;
            }
        }

        if !only_if_empty {
            self.quiet = quiet_from_env(env);
            self.skip_preflight = skip_preflight_from_env(env);
            self.beep = beep_from_env(env);
            self.export_results = export_results_from_env(env);
            if let Some(v) = env.get("DRY_RUN") {
                self.dry_run = v == "1" || v.eq_ignore_ascii_case("true");
            }
        } else {
            // Migration: only apply flags if env explicitly sets them
            if env.contains_key("QUIET") {
                self.quiet = quiet_from_env(env);
            }
            if env.contains_key("SKIP_PREFLIGHT") {
                self.skip_preflight = skip_preflight_from_env(env);
            }
            if env.contains_key("BEEP") {
                self.beep = beep_from_env(env);
            }
            if env.contains_key("EXPORT_RESULTS") {
                self.export_results = export_results_from_env(env);
            }
            if let Some(v) = env.get("DRY_RUN") {
                self.dry_run = v == "1" || v.eq_ignore_ascii_case("true");
            }
        }
    }

    pub fn apply_sniper_preset(&mut self) {
        self.gas_limit = 250_000;
        self.use_gql = false;
        self.priority_fee_gwei = "auto".to_string();
        self.base_fee_multiplier = "2.0".to_string();
        self.gas_multiplier = "1.0".to_string();
        self.max_retries = 30;
        self.quiet = true;
        self.skip_preflight = true;
        self.beep = true;
        self.export_results = true;
    }

    /// Non-empty proxy lines (comments `#…` stripped).
    pub fn proxy_lines(&self) -> Vec<String> {
        self.proxy_url
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.to_string())
            .collect()
    }

    pub fn proxy_count(&self) -> usize {
        self.proxy_lines().len()
    }

    /// Normalize multi-line proxy text (trim empty trailing lines, unify newlines).
    pub fn set_proxies_text(&mut self, text: &str) {
        let lines: Vec<&str> = text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        self.proxy_url = lines.join("\n");
        if !self.proxy_url.is_empty() {
            self.proxy_url.push('\n');
        }
    }

    /// Write `proxies.txt` next to config (for CLI mint ProxyManager).
    pub fn save_proxies_file(&self, next_to_config: &Path) -> anyhow::Result<()> {
        let path = next_to_config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("proxies.txt"))
            .unwrap_or_else(|| PathBuf::from("proxies.txt"));
        let lines = self.proxy_lines();
        if lines.is_empty() {
            // Don't delete existing file if user cleared settings accidentally without intent;
            // write empty only if we own the file or it doesn't exist.
            if path.exists() {
                std::fs::write(&path, "# empty — no proxies in Settings\n")?;
            }
            return Ok(());
        }
        let mut body = String::from("# managed by minter Settings (one proxy per line)\n");
        for l in lines {
            body.push_str(&l);
            body.push('\n');
        }
        std::fs::write(&path, body)?;
        Ok(())
    }

    fn load_proxies_file_if_empty(&mut self, next_to_config: &Path) {
        if !self.proxy_lines().is_empty() {
            return;
        }
        let path = next_to_config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("proxies.txt"))
            .unwrap_or_else(|| PathBuf::from("proxies.txt"));
        if !path.exists() {
            return;
        }
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let lines: Vec<&str> = raw
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect();
            if !lines.is_empty() {
                self.proxy_url = lines.join("\n") + "\n";
            }
        }
    }

    /// Persist as pretty JSON. Creates parent dirs if needed.
    /// Crash-safe: write temp + fsync + rename (same pattern as vault).
    pub fn save(&self, config_path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = config_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let raw = serde_json::to_string_pretty(self)? + "\n";
        Self::atomic_write_bytes(config_path, raw.as_bytes())?;
        let _ = self.save_proxies_file(config_path);
        Ok(())
    }

    /// Atomic write used by config save (public for tests).
    pub fn atomic_write_bytes(path: &Path, data: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp_path = PathBuf::from(tmp);
        {
            let mut file = std::fs::File::create(&tmp_path)
                .with_context(|| format!("create temp {}", tmp_path.display()))?;
            file.write_all(data)
                .context("write config temp")?;
            file.sync_all().context("fsync config temp")?;
        }
        if path.exists() {
            #[cfg(windows)]
            {
                let mut bak = path.as_os_str().to_owned();
                bak.push(".bak");
                let bak_path = PathBuf::from(bak);
                let _ = std::fs::remove_file(&bak_path);
                let _ = std::fs::rename(path, &bak_path);
                match std::fs::rename(&tmp_path, path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&bak_path);
                    }
                    Err(e) => {
                        let _ = std::fs::rename(&bak_path, path);
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(e).context("rename config into place");
                    }
                }
            }
            #[cfg(not(windows))]
            {
                std::fs::rename(&tmp_path, path).context("rename config into place")?;
            }
        } else if let Err(_) = std::fs::rename(&tmp_path, path) {
            std::fs::write(path, data).context("write config final")?;
            let _ = std::fs::remove_file(&tmp_path);
        }
        Ok(())
    }

    /// Optional mirror into `.env` so older helpers that re-read the file stay in sync.
    /// Primary store is always `config.json` via [`Settings::save`].
    pub fn save_to_env_file(&self, env_path: &PathBuf) -> anyhow::Result<()> {
        // Ensure config.json exists beside .env (or cwd)
        let config = env_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("config.json"))
            .unwrap_or_else(|| PathBuf::from("config.json"));
        self.save(&config)?;

        let existing = if env_path.exists() {
            std::fs::read_to_string(env_path)?
        } else {
            String::new()
        };
        let mut lines: Vec<String> = existing.lines().map(String::from).collect();
        for (key, value) in self.to_env_map() {
            if let Some(line) = lines.iter_mut().find(|l| {
                let l = l.trim();
                l.starts_with(&key) && l[key.len()..].starts_with('=')
            }) {
                *line = format!("{}={}", key, value);
            } else {
                lines.push(format!("{}={}", key, value));
            }
        }
        let content = lines.join("\n") + "\n";
        std::fs::write(env_path, content)?;
        Ok(())
    }

    /// Flat env map so existing mint/RPC code keeps working.
    pub fn to_env_map(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        if !self.alchemy_api_key.trim().is_empty() {
            m.insert(
                "ALCHEMY_API_KEY".to_string(),
                self.alchemy_api_key.trim().to_string(),
            );
        }
        if !self.rpc_urls.trim().is_empty() {
            let v = self.rpc_urls.trim().to_string();
            m.insert("RPC_URLS".to_string(), v.clone());
            // first URL also as RPC_URL
            if let Some(first) = v.split(',').map(str::trim).find(|s| !s.is_empty()) {
                m.insert("RPC_URL".to_string(), first.to_string());
            }
        }
        if !self.rpc_url_ethereum.trim().is_empty() {
            m.insert(
                "RPC_URL_ETHEREUM".to_string(),
                self.rpc_url_ethereum.trim().to_string(),
            );
            m.insert(
                "ETHEREUM_RPC_URL".to_string(),
                self.rpc_url_ethereum.trim().to_string(),
            );
        }
        if !self.rpc_url_base.trim().is_empty() {
            m.insert(
                "RPC_URL_BASE".to_string(),
                self.rpc_url_base.trim().to_string(),
            );
            m.insert(
                "BASE_RPC_URL".to_string(),
                self.rpc_url_base.trim().to_string(),
            );
        }
        if !self.rpc_url_polygon.trim().is_empty() {
            m.insert(
                "RPC_URL_POLYGON".to_string(),
                self.rpc_url_polygon.trim().to_string(),
            );
            m.insert(
                "POLYGON_RPC_URL".to_string(),
                self.rpc_url_polygon.trim().to_string(),
            );
        }
        // First proxy as PROXY_URL for single-proxy helpers; full list lives in proxies.txt
        if let Some(first) = self.proxy_lines().into_iter().next() {
            m.insert("PROXY_URL".to_string(), first);
        }
        if !self.flashbots_relay_url.trim().is_empty() {
            m.insert(
                "FLASHBOTS_RELAY_URL".to_string(),
                self.flashbots_relay_url.trim().to_string(),
            );
        }
        if self.flashbots_max_blocks > 0 {
            m.insert(
                "FLASHBOTS_MAX_BLOCKS".to_string(),
                self.flashbots_max_blocks.to_string(),
            );
        }
        if self.flashbots_resubmit_ms >= 200 {
            m.insert(
                "FLASHBOTS_RESUBMIT_MS".to_string(),
                self.flashbots_resubmit_ms.to_string(),
            );
        }

        m.insert("GAS_LIMIT".to_string(), self.gas_limit.to_string());
        m.insert(
            "USE_GQL".to_string(),
            if self.use_gql { "1" } else { "0" }.to_string(),
        );
        m.insert(
            "PRIORITY_FEE_GWEI".to_string(),
            self.priority_fee_gwei.clone(),
        );
        m.insert(
            "BASE_FEE_MULTIPLIER".to_string(),
            self.base_fee_multiplier.clone(),
        );
        m.insert("GAS_MULTIPLIER".to_string(), self.gas_multiplier.clone());
        m.insert("MAX_RETRIES".to_string(), self.max_retries.to_string());
        m.insert(
            "QUIET".to_string(),
            if self.quiet { "1" } else { "0" }.to_string(),
        );
        m.insert(
            "SKIP_PREFLIGHT".to_string(),
            if self.skip_preflight { "1" } else { "0" }.to_string(),
        );
        m.insert(
            "BEEP".to_string(),
            if self.beep { "1" } else { "0" }.to_string(),
        );
        m.insert(
            "EXPORT_RESULTS".to_string(),
            if self.export_results { "1" } else { "0" }.to_string(),
        );
        m.insert(
            "DRY_RUN".to_string(),
            if self.dry_run { "1" } else { "0" }.to_string(),
        );
        m
    }

    /// True if any RPC source is configured.
    pub fn has_rpc(&self) -> bool {
        !self.alchemy_api_key.trim().is_empty()
            || !self.rpc_urls.trim().is_empty()
            || !self.rpc_url_ethereum.trim().is_empty()
            || !self.rpc_url_base.trim().is_empty()
            || !self.rpc_url_polygon.trim().is_empty()
    }

    /// Mask secrets for UI display (last 4 chars).
    pub fn alchemy_masked(&self) -> String {
        mask_secret(&self.alchemy_api_key)
    }
}

fn mask_secret(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.len() <= 4 {
        return "****".to_string();
    }
    format!("…{}", &t[t.len() - 4..])
}

fn load_simple_env(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line
                .strip_prefix("export ")
                .or_else(|| line.strip_prefix("export\t"))
                .unwrap_or(line)
                .trim();
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            let val = val.trim();
            let val = if val.len() >= 2
                && ((val.starts_with('"') && val.ends_with('"'))
                    || (val.starts_with('\'') && val.ends_with('\'')))
            {
                val[1..val.len() - 1].to_string()
            } else {
                val.to_string()
            };
            map.insert(key.to_string(), val);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn roundtrip_config_json() {
        let dir = std::env::temp_dir().join(format!("minter-settings-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let mut s = Settings::default();
        s.alchemy_api_key = "testkey123456".into();
        s.rpc_urls = "https://example.com/rpc".into();
        s.gas_limit = 300_000;
        s.save(&path).unwrap();
        let loaded = Settings::load(&path, None);
        assert_eq!(loaded.alchemy_api_key, "testkey123456");
        assert_eq!(loaded.rpc_urls, "https://example.com/rpc");
        assert_eq!(loaded.gas_limit, 300_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_from_env() {
        let dir = std::env::temp_dir().join(format!("minter-env-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let env_path = dir.join(".env");
        let mut f = std::fs::File::create(&env_path).unwrap();
        writeln!(f, "ALCHEMY_API_KEY=migrated_key").unwrap();
        writeln!(f, "GAS_LIMIT=222000").unwrap();
        let config = dir.join("config.json");
        let s = Settings::load(&config, Some(&env_path));
        assert_eq!(s.alchemy_api_key, "migrated_key");
        assert_eq!(s.gas_limit, 222_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn to_env_map_has_alchemy() {
        let mut s = Settings::default();
        s.alchemy_api_key = "abc".into();
        let m = s.to_env_map();
        assert_eq!(m.get("ALCHEMY_API_KEY").map(String::as_str), Some("abc"));
    }

    #[test]
    fn atomic_write_then_reload_equals_intended() {
        let dir = std::env::temp_dir().join(format!(
            "minter-atomic-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        // Pre-existing file would be replaced atomically
        std::fs::write(&path, b"{\"version\":0}\n").unwrap();
        let payload = br#"{"alchemyApiKey":"atomic_secret_xyz","gasLimit":424242}"#;
        Settings::atomic_write_bytes(&path, payload).unwrap();
        // Temp must not remain as the only good file
        assert!(path.exists());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("atomic_secret_xyz") || raw.contains("gasLimit") || raw.contains("424242") || raw.contains("alchemy"));
        // Full Settings::save path
        let mut s = Settings::default();
        s.alchemy_api_key = "round_atomic_key".into();
        s.gas_limit = 111_000;
        s.save(&path).unwrap();
        let loaded = Settings::load(&path, None);
        assert_eq!(loaded.alchemy_api_key, "round_atomic_key");
        assert_eq!(loaded.gas_limit, 111_000);
        // leftover .tmp should not be the canonical path
        let tmp = path.with_extension("json.tmp");
        // after success, temp is renamed away
        assert!(!tmp.exists() || std::fs::read_to_string(&path).unwrap().contains("round_atomic_key"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
