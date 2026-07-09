use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use anyhow::{Context, Result};
use pbkdf2::pbkdf2_hmac;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;

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
    path: std::path::PathBuf,
    password: Option<String>,
}

impl AuthCache {
    pub fn load(password: Option<&str>) -> Self {
        let path = std::path::PathBuf::from(CACHE_FILE);
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
            password: password.map(|s| s.to_string()),
        }
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
        if let Some(pw) = &self.password {
            let json = serde_json::to_vec(&self.tokens).unwrap_or_default();
            let blob = Self::encrypt_tokens(&json, pw);
            let _ = std::fs::write(&self.path, &blob);
        }
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
