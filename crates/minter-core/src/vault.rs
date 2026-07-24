use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use alloy_primitives::Address;
use anyhow::{Context, Result, bail};
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::types::*;

pub struct Vault {
    path: PathBuf,
}

impl Vault {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> Self {
        Self::new(VAULT_FILE)
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Human path for operator messages.
    pub fn path_display(&self) -> String {
        self.path.display().to_string()
    }

    fn derive_key(&self, password: &str, salt: &[u8]) -> [u8; 32] {
        let mut key = [0u8; 32];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, VAULT_KDF_ITERATIONS, &mut key);
        key
    }

    fn encrypt(&self, data: &[u8], password: &str) -> Vec<u8> {
        let salt: [u8; VAULT_SALT_LEN] = {
            let mut s = [0u8; VAULT_SALT_LEN];
            getrandom::getrandom(&mut s).expect("rng failed");
            s
        };
        let key = self.derive_key(password, &salt);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let cipher = Aes256Gcm::new_from_slice(&key).expect("valid key");
        let ciphertext = cipher.encrypt(&nonce, data).expect("encryption succeeded");
        let mut out =
            Vec::with_capacity(VAULT_SALT_LEN + VAULT_IV_LEN + VAULT_TAG_LEN + ciphertext.len());
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        out.extend(ciphertext);
        out
    }

    fn decrypt(&self, blob: &[u8], password: &str) -> Result<Vec<u8>> {
        if blob.len() < VAULT_SALT_LEN + VAULT_IV_LEN + VAULT_TAG_LEN {
            bail!(
                "Vault file is too small or corrupted ({})",
                self.path.display()
            );
        }
        let salt = &blob[..VAULT_SALT_LEN];
        let nonce_bytes = &blob[VAULT_SALT_LEN..VAULT_SALT_LEN + VAULT_IV_LEN];
        let nonce = Nonce::from_slice(nonce_bytes);
        let ciphertext = &blob[VAULT_SALT_LEN + VAULT_IV_LEN..];
        let key = self.derive_key(password, salt);
        let cipher = Aes256Gcm::new_from_slice(&key).context("invalid key")?;
        let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_| {
            anyhow::anyhow!(
                "Wrong vault password (or corrupted vault file at {})",
                self.path.display()
            )
        })?;
        Ok(plaintext)
    }

    fn read_entries(&self, password: &str) -> Result<Vec<VaultEntry>> {
        if !self.exists() {
            // Empty vault is OK for first-time create (add/import). Callers that
            // expect an existing vault should check exists() first.
            return Ok(vec![]);
        }
        let blob = std::fs::read(&self.path).with_context(|| {
            format!(
                "Failed to read vault file {} (check path / permissions)",
                self.path.display()
            )
        })?;
        let plaintext = self.decrypt(&blob, password)?;
        let entries: Vec<VaultEntry> = serde_json::from_slice(&plaintext).with_context(|| {
            format!(
                "Vault file {} could not be parsed (corrupted?)",
                self.path.display()
            )
        })?;
        Ok(entries)
    }

    /// Write `data` to `self.path` via temp file + fsync + rename so a crash
    /// mid-write cannot leave a truncated/corrupt vault.
    ///
    /// On POSIX, `rename` replaces the destination atomically on the same FS.
    /// On Windows, rename cannot overwrite, so the previous file is moved aside
    /// first and restored if the final rename fails.
    fn atomic_write(&self, data: &[u8]) -> Result<()> {
        let parent = self.path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(dir) = parent {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create vault dir {}", dir.display()))?;
        }

        // Same directory as the vault so rename stays on one filesystem.
        let mut tmp_path = self.path.as_os_str().to_owned();
        tmp_path.push(".tmp");
        let tmp_path = PathBuf::from(tmp_path);

        {
            let mut file = std::fs::File::create(&tmp_path)
                .with_context(|| format!("failed to create vault temp {}", tmp_path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .context("failed to set vault temp permissions")?;
            }
            file.write_all(data).context("failed to write vault temp")?;
            file.sync_all().context("failed to fsync vault temp")?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
        }

        if !self.path.exists() {
            std::fs::rename(&tmp_path, &self.path).with_context(|| {
                format!(
                    "failed to rename vault temp {} -> {}",
                    tmp_path.display(),
                    self.path.display()
                )
            })?;
            return Ok(());
        }

        // Destination exists: replace without truncating the live file in place.
        let mut bak_path = self.path.as_os_str().to_owned();
        bak_path.push(".bak");
        let bak_path = PathBuf::from(bak_path);
        let _ = std::fs::remove_file(&bak_path);

        std::fs::rename(&self.path, &bak_path).with_context(|| {
            format!(
                "failed to move existing vault aside {} -> {}",
                self.path.display(),
                bak_path.display()
            )
        })?;

        match std::fs::rename(&tmp_path, &self.path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&bak_path);
                Ok(())
            }
            Err(e) => {
                // Best-effort restore of the previous vault.
                let _ = std::fs::rename(&bak_path, &self.path);
                let _ = std::fs::remove_file(&tmp_path);
                Err(anyhow::anyhow!(
                    "failed to install new vault {}: {}; previous vault restored if possible",
                    self.path.display(),
                    e
                ))
            }
        }
    }

    fn write_entries(&self, entries: &[VaultEntry], password: &str) -> Result<()> {
        let json = serde_json::to_vec(entries).context("failed to serialize entries")?;
        let encrypted = self.encrypt(&json, password);
        self.atomic_write(&encrypted)
            .context("failed to write vault atomically")?;
        Ok(())
    }

    pub fn add(&self, private_key: &str, password: &str) -> Result<Address> {
        let pk = private_key.strip_prefix("0x").unwrap_or(private_key);
        let signer: Signer = pk.parse().context("invalid private key")?;
        let addr = signer.address();
        let created_new = !self.exists();
        let mut entries = self.read_entries(password)?;
        if created_new {
            crate::rlog!(
                "No vault at {} — creating a new encrypted vault",
                self.path.display()
            );
        }
        let addr_lower = format!("{:?}", addr).to_lowercase();
        for e in &entries {
            if e.address.to_lowercase() == addr_lower {
                crate::rlog!("Address {} already in vault, skipping", addr);
                return Ok(addr);
            }
        }
        entries.push(VaultEntry {
            address: format!("{:?}", addr),
            key: if private_key.starts_with("0x") {
                private_key.to_string()
            } else {
                format!("0x{}", private_key)
            },
        });
        self.write_entries(&entries, password)?;
        crate::rlog!("Added {} ({} keys total)", addr, entries.len());
        Ok(addr)
    }

    pub fn remove(&self, address: &str, password: &str) -> Result<()> {
        if !self.exists() {
            bail!(
                "No vault file at {} — nothing to remove (wrong working directory?)",
                self.path.display()
            );
        }
        let addr_lower = address.to_lowercase();
        let mut entries = self.read_entries(password)?;
        let before = entries.len();
        entries.retain(|e| e.address.to_lowercase() != addr_lower);
        if entries.len() == before {
            crate::rlog!("Address {} not found in vault", address);
            return Ok(());
        }
        self.write_entries(&entries, password)?;
        crate::rlog!("Removed {} ({} keys remaining)", address, entries.len());
        Ok(())
    }

    pub fn list_addresses(&self, password: &str) -> Result<Vec<String>> {
        if !self.exists() {
            // Empty list is fine; unlock path creates password session first.
            return Ok(vec![]);
        }
        let entries = self.read_entries(password)?;
        Ok(entries.iter().map(|e| e.address.clone()).collect())
    }

    pub fn decrypt_keys(&self, password: &str) -> Result<Vec<zeroize::Zeroizing<String>>> {
        if !self.exists() {
            // First-run: no vault yet → empty key set (Session creates on add).
            return Ok(vec![]);
        }
        let entries = self.read_entries(password)?;
        Ok(entries
            .iter()
            .map(|e| zeroize::Zeroizing::new(e.key.clone()))
            .collect())
    }

    /// Import private keys from free-form text (one key per line; `#` comments ok).
    pub fn import_from_text(&self, content: &str, password: &str) -> Result<usize> {
        let created_new = !self.exists();
        let mut entries = self.read_entries(password)?;
        if created_new {
            crate::rlog!(
                "No vault at {} — creating a new encrypted vault on import",
                self.path.display()
            );
        }
        let mut seen: std::collections::HashSet<String> =
            entries.iter().map(|e| e.address.to_lowercase()).collect();
        let mut added = 0;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Support "address,key" or "key" only
            let key_part = if line.contains(',') {
                line.split(',').last().unwrap_or(line).trim()
            } else {
                line
            };
            let pk_owned = if key_part.starts_with("0x") || key_part.starts_with("0X") {
                key_part.to_string()
            } else {
                format!("0x{}", key_part)
            };
            let signer: Signer = match pk_owned.parse() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let addr = format!("{:?}", signer.address());
            if seen.contains(&addr.to_lowercase()) {
                continue;
            }
            entries.push(VaultEntry {
                address: addr.clone(),
                key: pk_owned,
            });
            seen.insert(addr.to_lowercase());
            added += 1;
        }
        if added == 0 {
            crate::rlog!("No new keys to import");
            return Ok(0);
        }
        self.write_entries(&entries, password)?;
        crate::rlog!("Imported {} new key(s) ({} total)", added, entries.len());
        Ok(added)
    }

    pub fn import_from_file(&self, filepath: &Path, password: &str) -> Result<usize> {
        let content = std::fs::read_to_string(filepath).context("failed to read import file")?;
        self.import_from_text(&content, password)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_vault_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "minter_vault_test_{}_{}_{}",
            std::process::id(),
            n,
            label
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("keys.vault")
    }

    fn cleanup_vault_path(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let vault = Vault::new("test_vault_tmp.bin");
        let password = "test_password_123";
        let plaintext = b"{\"test\":\"data\"}";

        let encrypted = vault.encrypt(plaintext, password);
        assert_ne!(&encrypted[..], plaintext);

        let decrypted = vault.decrypt(&encrypted, password).unwrap();
        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn decrypt_wrong_password_fails() {
        let vault = Vault::new("test_vault_tmp.bin");
        let encrypted = vault.encrypt(b"secret", "correct_pw");

        let err = vault.decrypt(&encrypted, "wrong_pw").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("wrong vault password"),
            "expected clear wrong-password message, got: {msg}"
        );
    }

    #[test]
    fn remove_missing_vault_is_clear_error() {
        let path = unique_vault_path("missing");
        let vault = Vault::new(&path);
        let err = vault.remove("0xabc", "pw").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No vault file") || msg.contains("nothing to remove"),
            "{msg}"
        );
        cleanup_vault_path(&path);
    }

    #[test]
    fn write_entries_atomic_roundtrip() {
        let path = unique_vault_path("roundtrip");
        let vault = Vault::new(&path);
        let password = "atomic_pw_1";

        let entries = vec![VaultEntry {
            address: "0xabc".to_string(),
            key: "0xdead".to_string(),
        }];
        vault.write_entries(&entries, password).unwrap();

        assert!(path.exists());
        // Temp / bak leftovers should not remain after success.
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists());
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        assert!(!PathBuf::from(bak).exists());

        let loaded = vault.read_entries(password).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].address, "0xabc");
        assert_eq!(loaded[0].key, "0xdead");

        cleanup_vault_path(&path);
    }

    #[test]
    fn write_entries_overwrite_preserves_readable_vault() {
        let path = unique_vault_path("overwrite");
        let vault = Vault::new(&path);
        let password = "atomic_pw_2";

        vault
            .write_entries(
                &[VaultEntry {
                    address: "0x1".to_string(),
                    key: "0xaaa".to_string(),
                }],
                password,
            )
            .unwrap();

        vault
            .write_entries(
                &[
                    VaultEntry {
                        address: "0x1".to_string(),
                        key: "0xaaa".to_string(),
                    },
                    VaultEntry {
                        address: "0x2".to_string(),
                        key: "0xbbb".to_string(),
                    },
                ],
                password,
            )
            .unwrap();

        let loaded = vault.read_entries(password).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].address, "0x2");

        // File must be non-empty encrypted blob, not truncated garbage.
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() > 32);

        cleanup_vault_path(&path);
    }

    #[test]
    fn atomic_write_bytes_roundtrip() {
        let path = unique_vault_path("bytes");
        let vault = Vault::new(&path);
        let payload = b"not-empty-vault-payload-bytes";
        vault.atomic_write(payload).unwrap();
        let read = std::fs::read(&path).unwrap();
        assert_eq!(read, payload);
        cleanup_vault_path(&path);
    }
}
