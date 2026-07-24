use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use anyhow::{Context, Result};
use pbkdf2::pbkdf2_hmac;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use crate::types::{VAULT_IV_LEN, VAULT_KDF_ITERATIONS, VAULT_SALT_LEN};

#[derive(Serialize, Deserialize, Clone)]
struct CachedToken {
    access_token: String,
    address: String,
    chain_id: u64,
    expires_at: i64,
}

const CACHE_FILE: &str = "auth_cache.bin";
const TOKEN_TTL_SECS: i64 = 3000;

pub struct AuthCache {
    tokens: HashMap<String, CachedToken>,
    path: PathBuf,
    /// Vault password for encrypt-at-rest; zeroized on drop.
    password: Option<Zeroizing<String>>,
    /// True when in-memory tokens differ from last successful disk flush.
    dirty: bool,
}

impl AuthCache {
    pub fn load(password: Option<&str>) -> Self {
        Self::load_at(PathBuf::from(CACHE_FILE), password)
    }

    /// Load from an explicit path (tests / alternate data dirs).
    pub fn load_at(path: impl Into<PathBuf>, password: Option<&str>) -> Self {
        let path = path.into();
        let tokens = if let Some(pw) = password {
            if path.exists() {
                match std::fs::read(&path) {
                    Ok(blob) => Self::decrypt_tokens(&blob, pw).unwrap_or_default(),
                    Err(_) => HashMap::new(),
                }
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };
        Self {
            tokens,
            path,
            password: password.map(|s| Zeroizing::new(s.to_string())),
            dirty: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, address: &str, chain_id: u64) -> Option<&str> {
        let key = format!("{}:{}", address.to_lowercase(), chain_id);
        let cached = self.tokens.get(&key)?;
        let now = chrono::Utc::now().timestamp();
        if now >= cached.expires_at {
            return None;
        }
        Some(&cached.access_token)
    }

    /// Insert/update token in memory only. Call [`flush`] once after a batch of saves
    /// so PBKDF2 + encrypt runs at most once (not per wallet).
    pub fn save(&mut self, address: &str, chain_id: u64, access_token: &str) {
        let key = format!("{}:{}", address.to_lowercase(), chain_id);
        let now = chrono::Utc::now().timestamp();
        self.tokens.insert(
            key,
            CachedToken {
                access_token: access_token.to_string(),
                address: address.to_string(),
                chain_id,
                expires_at: now + TOKEN_TTL_SECS,
            },
        );
        self.dirty = true;
    }

    /// Persist encrypted cache to disk if dirty and a password is set.
    /// Returns `true` if a disk write was performed.
    pub fn flush(&mut self) -> Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        let Some(pw) = &self.password else {
            // No password → memory-only cache; mark clean so we don't retry forever.
            self.dirty = false;
            return Ok(false);
        };
        let json = serde_json::to_vec(&self.tokens).context("serialize auth cache")?;
        let blob = Self::encrypt_tokens(&json, pw.as_str());
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        // Atomic write with 0600 (audit L6): temp in same dir → fsync → rename,
        // so a crash mid-write can't leave a truncated cache, and the encrypted
        // blob isn't world-readable on unix (parity with the vault writer).
        let mut tmp = self.path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("create auth cache temp {}", tmp.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
            }
            std::io::Write::write_all(&mut f, &blob).context("write auth cache temp")?;
            f.sync_all().context("fsync auth cache temp")?;
        }
        if std::fs::rename(&tmp, &self.path).is_err() {
            // Windows can't always rename over an existing file; fall back to a
            // direct write and clean up the temp.
            std::fs::write(&self.path, &blob)
                .with_context(|| format!("write auth cache {}", self.path.display()))?;
            let _ = std::fs::remove_file(&tmp);
        }
        self.dirty = false;
        Ok(true)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Drop all cached SIWE tokens (memory + encrypted file).
    pub fn clear(&mut self) -> Result<()> {
        self.tokens.clear();
        self.dirty = false;
        if self.path.exists() {
            std::fs::remove_file(&self.path)
                .with_context(|| format!("remove auth cache {}", self.path.display()))?;
        }
        // also remove legacy plaintext if present
        let legacy = std::path::Path::new("auth_cache.json");
        if legacy.exists() {
            let _ = std::fs::remove_file(legacy);
        }
        Ok(())
    }

    fn derive_key(password: &str, salt: &[u8]) -> [u8; 32] {
        let mut key = [0u8; 32];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, VAULT_KDF_ITERATIONS, &mut key);
        key
    }

    fn encrypt_tokens(data: &[u8], password: &str) -> Vec<u8> {
        let mut salt = [0u8; VAULT_SALT_LEN];
        getrandom::getrandom(&mut salt).expect("rng failed");
        let key = Self::derive_key(password, &salt);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let cipher = Aes256Gcm::new_from_slice(&key).expect("valid key");
        let ciphertext = cipher.encrypt(&nonce, data).expect("encryption succeeded");
        let mut out = Vec::with_capacity(VAULT_SALT_LEN + VAULT_IV_LEN + ciphertext.len());
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        out.extend(ciphertext);
        out
    }

    fn decrypt_tokens(blob: &[u8], password: &str) -> Result<HashMap<String, CachedToken>> {
        if blob.len() < VAULT_SALT_LEN + VAULT_IV_LEN {
            anyhow::bail!("auth cache file too small or corrupted");
        }
        let salt = &blob[..VAULT_SALT_LEN];
        let nonce_bytes = &blob[VAULT_SALT_LEN..VAULT_SALT_LEN + VAULT_IV_LEN];
        let nonce = Nonce::from_slice(nonce_bytes);
        let ciphertext = &blob[VAULT_SALT_LEN + VAULT_IV_LEN..];
        let key = Self::derive_key(password, salt);
        let cipher = Aes256Gcm::new_from_slice(&key).context("invalid key")?;
        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("auth cache decrypt failed: {}", e))?;
        Ok(serde_json::from_slice(&plaintext).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_cache_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "minter_auth_cache_{}_{}_{}",
            std::process::id(),
            n,
            label
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("auth_cache.bin")
    }

    #[test]
    fn save_is_memory_only_until_flush() {
        let path = unique_cache_path("mem");
        let pw = "test_pw_auth";
        let mut cache = AuthCache::load_at(&path, Some(pw));
        cache.save("0xAbc", 1, "token_a");
        cache.save("0xDef", 1, "token_b");
        assert!(cache.is_dirty());
        assert!(!path.exists(), "disk write must wait for flush");
        assert!(cache.flush().unwrap());
        assert!(!cache.is_dirty());
        assert!(path.exists());
        // second flush is no-op
        assert!(!cache.flush().unwrap());

        let loaded = AuthCache::load_at(&path, Some(pw));
        assert_eq!(loaded.get("0xabc", 1), Some("token_a"));
        assert_eq!(loaded.get("0xdef", 1), Some("token_b"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn multi_save_one_flush_roundtrip() {
        let path = unique_cache_path("batch");
        let pw = "batch_pw";
        let mut cache = AuthCache::load_at(&path, Some(pw));
        for i in 0..5 {
            cache.save(&format!("0x{i:040x}"), 8453, &format!("tok{i}"));
        }
        assert_eq!(cache.len(), 5);
        cache.flush().unwrap();
        let loaded = AuthCache::load_at(&path, Some(pw));
        assert_eq!(loaded.len(), 5);
        assert_eq!(loaded.get(&format!("0x{:040x}", 3), 8453), Some("tok3"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
