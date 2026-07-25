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
/// Fallback lifetime when the token carries no readable `exp` claim.
const TOKEN_TTL_SECS: i64 = 3000;
/// Safety margin subtracted from a JWT's own `exp` so a token isn't used in the
/// last moments before the server rejects it.
const TOKEN_EXPIRY_SKEW_SECS: i64 = 30;

/// Decode a base64url segment (no padding) without pulling in a base64 crate.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = val(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Read the `exp` (unix seconds) claim out of a JWT access token.
///
/// OpenSea's token carries its own lifetime; assuming a fixed TTL means a token
/// the server has already rejected can still look "fresh" in the cache.
fn jwt_exp_secs(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = b64url_decode(payload)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(|e| e.as_i64())
}

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
        // An empty token is not a usable session — force a re-auth rather than
        // reporting a cache hit that yields unauthenticated requests.
        if cached.access_token.trim().is_empty() {
            return None;
        }
        Some(&cached.access_token)
    }

    /// Insert/update token in memory only. Call [`flush`] once after a batch of saves
    /// so PBKDF2 + encrypt runs at most once (not per wallet).
    pub fn save(&mut self, address: &str, chain_id: u64, access_token: &str) {
        // Never persist an empty bearer: it would look like a valid cache hit
        // for the whole TTL and silently downgrade the wallet to anonymous.
        if access_token.trim().is_empty() {
            return;
        }
        let key = format!("{}:{}", address.to_lowercase(), chain_id);
        let now = chrono::Utc::now().timestamp();
        // Prefer the token's own `exp`. A hard-coded 3000s TTL is a guess: if the
        // real lifetime is shorter, the cache keeps serving a token the server
        // already rejects, and every request 401s until the TTL runs out.
        let expires_at = jwt_exp_secs(access_token)
            .map(|exp| exp - TOKEN_EXPIRY_SKEW_SECS)
            .filter(|exp| *exp > now)
            .unwrap_or(now + TOKEN_TTL_SECS);
        self.tokens.insert(
            key,
            CachedToken {
                access_token: access_token.to_string(),
                address: address.to_string(),
                chain_id,
                expires_at,
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

    /// Derive the AES-256 key. Returned in `Zeroizing` so the master key is
    /// scrubbed from the stack instead of lingering in freed memory.
    fn derive_key(password: &str, salt: &[u8]) -> Zeroizing<[u8; 32]> {
        let mut key = Zeroizing::new([0u8; 32]);
        pbkdf2_hmac::<Sha256>(
            password.as_bytes(),
            salt,
            VAULT_KDF_ITERATIONS,
            key.as_mut(),
        );
        key
    }

    fn encrypt_tokens(data: &[u8], password: &str) -> Vec<u8> {
        let mut salt = [0u8; VAULT_SALT_LEN];
        getrandom::getrandom(&mut salt).expect("rng failed");
        let key = Self::derive_key(password, &salt);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let cipher = Aes256Gcm::new_from_slice(key.as_ref()).expect("valid key");
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
        let cipher = Aes256Gcm::new_from_slice(key.as_ref()).context("invalid key")?;
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

    /// Build an unsigned JWT with the given `exp` (only the payload is read).
    fn jwt_with_exp(exp: i64) -> String {
        fn b64url(bytes: &[u8]) -> String {
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for c in bytes.chunks(3) {
                let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                let idx = [n >> 18 & 63, n >> 12 & 63, n >> 6 & 63, n & 63];
                for (i, ix) in idx.iter().enumerate() {
                    if i <= c.len() {
                        out.push(T[*ix as usize] as char);
                    }
                }
            }
            out
        }
        let payload = format!("{{\"exp\":{exp}}}");
        format!("header.{}.sig", b64url(payload.as_bytes()))
    }

    #[test]
    fn jwt_exp_is_parsed_and_preferred_over_fixed_ttl() {
        let now = chrono::Utc::now().timestamp();
        // Token that expires in 60s — far shorter than the 3000s fallback.
        let short = jwt_with_exp(now + 60);
        assert_eq!(jwt_exp_secs(&short), Some(now + 60));

        let dir = std::env::temp_dir().join(format!("ac_jwt_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut c = AuthCache::load_at(dir.join("c.bin"), None);
        c.save("0xabc", 1, &short);
        // Must expire per the JWT, not now+3000.
        let exp = c.tokens.get("0xabc:1").unwrap().expires_at;
        assert!(
            exp <= now + 60 && exp > now,
            "expiry {exp} should track the JWT exp ({}), not the {TOKEN_TTL_SECS}s TTL",
            now + 60
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opaque_token_falls_back_to_fixed_ttl() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(jwt_exp_secs("not-a-jwt"), None);
        assert_eq!(jwt_exp_secs(""), None);

        let dir = std::env::temp_dir().join(format!("ac_opaque_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut c = AuthCache::load_at(dir.join("c.bin"), None);
        c.save("0xabc", 1, "opaque-token-value");
        let exp = c.tokens.get("0xabc:1").unwrap().expires_at;
        assert!(
            exp >= now + TOKEN_TTL_SECS - 2,
            "should use the fallback TTL"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn already_expired_jwt_does_not_poison_the_cache() {
        let now = chrono::Utc::now().timestamp();
        let dir = std::env::temp_dir().join(format!("ac_exp_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut c = AuthCache::load_at(dir.join("c.bin"), None);
        // exp in the past → fall back to the TTL rather than storing a dead entry.
        c.save("0xabc", 1, &jwt_with_exp(now - 5_000));
        assert!(
            c.get("0xabc", 1).is_some(),
            "must not store an already-dead token"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
