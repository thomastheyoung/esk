use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use zeroize::Zeroize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorePayload {
    pub secrets: BTreeMap<String, String>,
    pub version: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tombstones: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_versions: BTreeMap<String, u64>,
}

#[derive(Debug)]
pub struct SecretStore {
    key: Vec<u8>,
    store_path: PathBuf,
    #[allow(dead_code)]
    key_path: PathBuf,
}

impl Drop for SecretStore {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl SecretStore {
    /// Load existing store or create a new empty one.
    pub fn load_or_create(root: &Path) -> Result<Self> {
        let lockbox_dir = root.join(".lockbox");
        if !lockbox_dir.is_dir() {
            std::fs::create_dir_all(&lockbox_dir)
                .with_context(|| format!("failed to create {}", lockbox_dir.display()))?;
        }
        let store_path = lockbox_dir.join("store.enc");
        let key_path = lockbox_dir.join("store.key");

        let key = if key_path.is_file() {
            Self::read_key(&key_path)?
        } else {
            let k = Self::generate_key();
            Self::write_key(&key_path, &k)?;
            k
        };

        let store = Self {
            key,
            store_path,
            key_path,
        };

        // Create empty store file if it doesn't exist
        if !store.store_path.is_file() {
            let payload = StorePayload {
                secrets: BTreeMap::new(),
                version: 0,
                tombstones: BTreeMap::new(),
                env_versions: BTreeMap::new(),
            };
            store.write_payload(&payload)?;
        }

        Ok(store)
    }

    /// Open an existing store (errors if key or store file is missing).
    pub fn open(root: &Path) -> Result<Self> {
        let store_path = root.join(".lockbox/store.enc");
        let key_path = root.join(".lockbox/store.key");

        if !key_path.is_file() {
            bail!(
                "encryption key not found at {}. Run `lockbox init` first.",
                key_path.display()
            );
        }
        if !store_path.is_file() {
            bail!(
                "encrypted store not found at {}. Run `lockbox init` first.",
                store_path.display()
            );
        }

        let key = Self::read_key(&key_path)?;
        Ok(Self {
            key,
            store_path,
            key_path,
        })
    }

    #[allow(dead_code)]
    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    #[allow(dead_code)]
    pub fn key_path(&self) -> &Path {
        &self.key_path
    }

    /// Acquire an exclusive file lock on store.key, run the closure, then release.
    fn with_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R>,
    {
        let file = std::fs::File::open(&self.key_path)
            .with_context(|| format!("failed to open {} for locking", self.key_path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("failed to acquire lock on {}", self.key_path.display()))?;
        let result = f();
        // Lock released when `file` is dropped
        drop(file);
        result
    }

    /// Decrypt and return the full payload.
    pub fn payload(&self) -> Result<StorePayload> {
        let ciphertext = std::fs::read_to_string(&self.store_path)
            .with_context(|| format!("failed to read {}", self.store_path.display()))?;
        let ciphertext = ciphertext.trim();
        if ciphertext.is_empty() {
            return Ok(StorePayload {
                secrets: BTreeMap::new(),
                version: 0,
                tombstones: BTreeMap::new(),
                env_versions: BTreeMap::new(),
            });
        }
        self.decrypt(ciphertext)
    }

    /// Get a single secret by composite key (e.g., "MY_SECRET:dev").
    pub fn get(&self, key: &str, env: &str) -> Result<Option<String>> {
        let payload = self.payload()?;
        let composite = format!("{key}:{env}");
        Ok(payload.secrets.get(&composite).cloned())
    }

    /// Set a secret, incrementing both global and env-specific versions. Acquires exclusive lock.
    pub fn set(&self, key: &str, env: &str, value: &str) -> Result<StorePayload> {
        self.with_lock(|| {
            let mut payload = self.payload()?;
            let composite = format!("{key}:{env}");
            payload.secrets.insert(composite.clone(), value.to_string());
            payload.tombstones.remove(&composite);
            payload.version += 1;
            let env_v = payload.env_versions.entry(env.to_string()).or_insert(0);
            *env_v += 1;
            self.write_payload(&payload)?;
            Ok(payload)
        })
    }

    /// Delete a secret, adding a tombstone. Acquires exclusive lock.
    pub fn delete(&self, key: &str, env: &str) -> Result<StorePayload> {
        self.with_lock(|| {
            let mut payload = self.payload()?;
            let composite = format!("{key}:{env}");
            if payload.secrets.remove(&composite).is_none() {
                bail!("secret '{key}' has no value for environment '{env}'");
            }
            payload.version += 1;
            let env_v = payload.env_versions.entry(env.to_string()).or_insert(0);
            *env_v += 1;
            payload.tombstones.insert(composite, payload.version);
            self.write_payload(&payload)?;
            Ok(payload)
        })
    }

    /// Write a full payload under exclusive lock. Used by pull reconciliation.
    pub fn set_payload(&self, payload: &StorePayload) -> Result<()> {
        self.with_lock(|| self.write_payload(payload))
    }

    /// List all secrets (returns the full BTreeMap).
    pub fn list(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.payload()?.secrets)
    }

    /// Write a payload to the store, encrypting it.
    pub fn write_payload(&self, payload: &StorePayload) -> Result<()> {
        let json = serde_json::to_string(payload)?;
        let encrypted = self.encrypt(&json)?;

        let dir = self
            .store_path
            .parent()
            .context("store path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), &encrypted)?;
        tmp.persist(&self.store_path)
            .with_context(|| format!("failed to persist store to {}", self.store_path.display()))?;
        Ok(())
    }

    /// Encrypt arbitrary plaintext into nonce:ciphertext:tag hex format.
    pub fn encrypt_raw(&self, plaintext: &str) -> Result<String> {
        self.encrypt(plaintext)
    }

    /// Decrypt raw ciphertext (nonce:ciphertext:tag hex format) into a StorePayload.
    pub fn decrypt_raw(&self, encoded: &str) -> Result<StorePayload> {
        self.decrypt(encoded)
    }

    fn encrypt(&self, plaintext: &str) -> Result<String> {
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;

        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

        // AES-GCM appends tag to ciphertext. Split for our format.
        // aes-gcm crate: ciphertext includes the 16-byte tag at the end
        let tag_start = ciphertext.len() - 16;
        let ct = &ciphertext[..tag_start];
        let tag = &ciphertext[tag_start..];

        Ok(format!(
            "{}:{}:{}",
            hex::encode(nonce_bytes),
            hex::encode(ct),
            hex::encode(tag)
        ))
    }

    fn decrypt(&self, encoded: &str) -> Result<StorePayload> {
        let parts: Vec<&str> = encoded.split(':').collect();
        if parts.len() != 3 {
            bail!("invalid store format: expected nonce:ciphertext:tag");
        }

        let nonce_bytes = hex::decode(parts[0]).context("invalid nonce hex")?;
        let ct_bytes = hex::decode(parts[1]).context("invalid ciphertext hex")?;
        let tag_bytes = hex::decode(parts[2]).context("invalid tag hex")?;

        if nonce_bytes.len() != 12 {
            bail!(
                "invalid nonce length: expected 12, got {}",
                nonce_bytes.len()
            );
        }

        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;
        let nonce = Nonce::from_slice(&nonce_bytes);

        // Recombine ciphertext + tag for aes-gcm
        let mut combined = ct_bytes;
        combined.extend_from_slice(&tag_bytes);

        let plaintext = cipher
            .decrypt(nonce, combined.as_ref())
            .map_err(|_| anyhow::anyhow!("decryption failed — wrong key or corrupted store"))?;

        let json = String::from_utf8(plaintext).context("decrypted payload is not valid UTF-8")?;
        serde_json::from_str(&json).context("decrypted payload is not valid JSON")
    }

    fn generate_key() -> Vec<u8> {
        let mut key = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        key
    }

    fn read_key(path: &Path) -> Result<Vec<u8>> {
        let hex_str = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read key from {}", path.display()))?;
        hex::decode(hex_str.trim()).context("invalid key hex")
    }

    fn write_key(path: &Path, key: &[u8]) -> Result<()> {
        let dir = path.parent().context("key path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), hex::encode(key))?;
        // Restrict permissions before persisting so the file is never world-readable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
        }
        tmp.persist(path)
            .with_context(|| format!("failed to persist key to {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn load_or_create_fresh() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        assert!(dir.path().join(".lockbox/store.key").is_file());
        assert!(dir.path().join(".lockbox/store.enc").is_file());
        let payload = store.payload().unwrap();
        assert!(payload.secrets.is_empty());
        assert_eq!(payload.version, 0);
    }

    #[test]
    fn load_or_create_existing() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        let key_before = std::fs::read_to_string(dir.path().join(".lockbox/store.key")).unwrap();

        let store2 = SecretStore::load_or_create(dir.path()).unwrap();
        let key_after = std::fs::read_to_string(dir.path().join(".lockbox/store.key")).unwrap();
        assert_eq!(key_before, key_after);
        assert_eq!(store2.get("KEY", "dev").unwrap(), Some("val".to_string()));
    }

    #[test]
    fn load_or_create_key_exists_no_store() {
        let dir = tmp_root();
        // Create key only
        SecretStore::load_or_create(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(".lockbox/store.enc")).unwrap();

        let store = SecretStore::load_or_create(dir.path()).unwrap();
        assert!(dir.path().join(".lockbox/store.enc").is_file());
        let payload = store.payload().unwrap();
        assert_eq!(payload.version, 0);
    }

    #[test]
    fn open_missing_key() {
        let dir = tmp_root();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("encryption key not found"));
    }

    #[test]
    fn open_missing_store() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(".lockbox/store.enc")).unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("encrypted store not found"));
    }

    #[test]
    fn open_both_exist() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        SecretStore::open(dir.path()).unwrap();
    }

    #[test]
    fn set_and_get_roundtrip() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("API_KEY", "dev", "sk_test_123").unwrap();
        assert_eq!(
            store.get("API_KEY", "dev").unwrap(),
            Some("sk_test_123".to_string())
        );
    }

    #[test]
    fn get_nonexistent_key() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        assert_eq!(store.get("NOPE", "dev").unwrap(), None);
    }

    #[test]
    fn get_wrong_env() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        assert_eq!(store.get("KEY", "prod").unwrap(), None);
    }

    #[test]
    fn set_increments_version() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let p1 = store.set("A", "dev", "1").unwrap();
        let p2 = store.set("B", "dev", "2").unwrap();
        let p3 = store.set("C", "dev", "3").unwrap();
        assert_eq!(p1.version, 1);
        assert_eq!(p2.version, 2);
        assert_eq!(p3.version, 3);
    }

    #[test]
    fn set_overwrites_existing() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "old").unwrap();
        store.set("KEY", "dev", "new").unwrap();
        assert_eq!(store.get("KEY", "dev").unwrap(), Some("new".to_string()));
    }

    #[test]
    fn list_empty_store() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_multiple_secrets() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("A", "dev", "1").unwrap();
        store.set("B", "prod", "2").unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.contains_key("A:dev"));
        assert!(list.contains_key("B:prod"));
    }

    #[test]
    fn payload_empty_file() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        // Overwrite the enc file with empty content
        std::fs::write(dir.path().join(".lockbox/store.enc"), "").unwrap();
        let payload = store.payload().unwrap();
        assert_eq!(payload.version, 0);
        assert!(payload.secrets.is_empty());
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let plaintext = r#"{"secrets":{"KEY:dev":"val"},"version":1}"#;
        let encrypted = store.encrypt(plaintext).unwrap();
        let decrypted = store.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted.secrets.get("KEY:dev").unwrap(), "val");
        assert_eq!(decrypted.version, 1);
    }

    #[test]
    fn decrypt_wrong_key() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let encrypted = store.encrypt(r#"{"secrets":{},"version":0}"#).unwrap();

        // Create a different key
        let dir2 = tmp_root();
        let store2 = SecretStore::load_or_create(dir2.path()).unwrap();
        let err = store2.decrypt(&encrypted).unwrap_err();
        assert!(err.to_string().contains("wrong key or corrupted"));
    }

    #[test]
    fn decrypt_invalid_format_no_colons() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.decrypt("nocolonshere").unwrap_err();
        assert!(err.to_string().contains("invalid store format"));
    }

    #[test]
    fn decrypt_invalid_format_two_parts() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.decrypt("aa:bb").unwrap_err();
        assert!(err.to_string().contains("invalid store format"));
    }

    #[test]
    fn decrypt_invalid_format_four_parts() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.decrypt("aa:bb:cc:dd").unwrap_err();
        assert!(err.to_string().contains("invalid store format"));
    }

    #[test]
    fn decrypt_invalid_nonce_hex() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.decrypt("zzzz:aabb:ccdd").unwrap_err();
        assert!(err.to_string().contains("invalid nonce hex"));
    }

    #[test]
    fn decrypt_invalid_ciphertext_hex() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.decrypt("aabb:zzzz:ccdd").unwrap_err();
        assert!(err.to_string().contains("invalid ciphertext hex"));
    }

    #[test]
    fn decrypt_invalid_tag_hex() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.decrypt("aabb:ccdd:zzzz").unwrap_err();
        assert!(err.to_string().contains("invalid tag hex"));
    }

    #[test]
    fn decrypt_wrong_nonce_length() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        // 8 bytes instead of 12
        let nonce = hex::encode([0u8; 8]);
        let ct = hex::encode([0u8; 16]);
        let tag = hex::encode([0u8; 16]);
        let err = store.decrypt(&format!("{nonce}:{ct}:{tag}")).unwrap_err();
        assert!(err.to_string().contains("invalid nonce length"));
    }

    #[test]
    fn decrypt_tampered_ciphertext() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let encrypted = store.encrypt(r#"{"secrets":{},"version":0}"#).unwrap();
        let parts: Vec<&str> = encrypted.split(':').collect();
        let mut ct_bytes = hex::decode(parts[1]).unwrap();
        if !ct_bytes.is_empty() {
            ct_bytes[0] ^= 0xFF;
        }
        let tampered = format!("{}:{}:{}", parts[0], hex::encode(&ct_bytes), parts[2]);
        assert!(store.decrypt(&tampered).is_err());
    }

    #[test]
    fn decrypt_tampered_tag() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let encrypted = store.encrypt(r#"{"secrets":{},"version":0}"#).unwrap();
        let parts: Vec<&str> = encrypted.split(':').collect();
        let mut tag_bytes = hex::decode(parts[2]).unwrap();
        tag_bytes[0] ^= 0xFF;
        let tampered = format!("{}:{}:{}", parts[0], parts[1], hex::encode(&tag_bytes));
        assert!(store.decrypt(&tampered).is_err());
    }

    #[test]
    fn decrypt_tampered_nonce() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let encrypted = store.encrypt(r#"{"secrets":{},"version":0}"#).unwrap();
        let parts: Vec<&str> = encrypted.split(':').collect();
        let mut nonce_bytes = hex::decode(parts[0]).unwrap();
        nonce_bytes[0] ^= 0xFF;
        let tampered = format!("{}:{}:{}", hex::encode(&nonce_bytes), parts[1], parts[2]);
        assert!(store.decrypt(&tampered).is_err());
    }

    #[test]
    fn decrypt_truncated_ciphertext() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let encrypted = store.encrypt(r#"{"secrets":{},"version":0}"#).unwrap();
        let parts: Vec<&str> = encrypted.split(':').collect();
        let ct_bytes = hex::decode(parts[1]).unwrap();
        let truncated = &ct_bytes[..ct_bytes.len().saturating_sub(4).max(1)];
        let tampered = format!("{}:{}:{}", parts[0], hex::encode(truncated), parts[2]);
        assert!(store.decrypt(&tampered).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn key_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        let metadata = std::fs::metadata(dir.path().join(".lockbox/store.key")).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn key_is_32_bytes() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        let hex_str = std::fs::read_to_string(dir.path().join(".lockbox/store.key")).unwrap();
        let key_bytes = hex::decode(hex_str.trim()).unwrap();
        assert_eq!(key_bytes.len(), 32);
    }

    #[test]
    fn key_hex_roundtrip() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        let hex_str = std::fs::read_to_string(dir.path().join(".lockbox/store.key")).unwrap();
        let key_bytes = hex::decode(hex_str.trim()).unwrap();
        assert_eq!(hex::encode(&key_bytes), hex_str.trim());
    }

    #[test]
    fn write_payload_atomic() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        assert!(dir.path().join(".lockbox/store.enc").is_file());
        // No temp files left behind
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp"))
            .collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn multiple_encryptions_differ() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let plaintext = r#"{"secrets":{},"version":0}"#;
        let enc1 = store.encrypt(plaintext).unwrap();
        let enc2 = store.encrypt(plaintext).unwrap();
        assert_ne!(enc1, enc2); // Random nonce each time
    }

    #[test]
    fn invalid_key_hex_in_file() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        std::fs::write(dir.path().join(".lockbox/store.key"), "not_valid_hex_zzz").unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid key hex"));
    }

    #[test]
    fn empty_key_file() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        std::fs::write(dir.path().join(".lockbox/store.key"), "").unwrap();
        let store = SecretStore::open(dir.path()).unwrap();
        let err = store.encrypt("test").unwrap_err();
        assert!(err.to_string().contains("failed to create cipher"));
    }

    #[test]
    fn delete_removes_secret() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        let payload = store.delete("KEY", "dev").unwrap();
        assert_eq!(payload.version, 2);
        assert!(payload.secrets.get("KEY:dev").is_none());
        assert!(store.get("KEY", "dev").unwrap().is_none());
    }

    #[test]
    fn delete_adds_tombstone() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        let payload = store.delete("KEY", "dev").unwrap();
        assert_eq!(payload.tombstones.get("KEY:dev"), Some(&2));
    }

    #[test]
    fn delete_nonexistent_errors() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.delete("NOPE", "dev").unwrap_err();
        assert!(err.to_string().contains("no value for environment"));
    }

    #[test]
    fn delete_preserves_other_envs() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "dev_val").unwrap();
        store.set("KEY", "prod", "prod_val").unwrap();
        store.delete("KEY", "dev").unwrap();
        assert!(store.get("KEY", "dev").unwrap().is_none());
        assert_eq!(
            store.get("KEY", "prod").unwrap(),
            Some("prod_val".to_string())
        );
    }

    #[test]
    fn set_clears_tombstone() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        store.delete("KEY", "dev").unwrap();
        let payload = store.set("KEY", "dev", "new_val").unwrap();
        assert!(!payload.tombstones.contains_key("KEY:dev"));
    }

    #[test]
    fn tombstone_serialization_roundtrip() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("A", "dev", "val").unwrap();
        store.delete("A", "dev").unwrap();

        // Reload and verify tombstones survived
        let store2 = SecretStore::open(dir.path()).unwrap();
        let payload = store2.payload().unwrap();
        assert_eq!(payload.tombstones.get("A:dev"), Some(&2));
        assert!(payload.secrets.get("A:dev").is_none());
    }

    #[test]
    fn set_increments_env_version() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let p1 = store.set("A", "dev", "1").unwrap();
        assert_eq!(p1.env_versions.get("dev"), Some(&1));
        let p2 = store.set("B", "dev", "2").unwrap();
        assert_eq!(p2.env_versions.get("dev"), Some(&2));
        // Setting a prod key shouldn't increment dev version
        let p3 = store.set("C", "prod", "3").unwrap();
        assert_eq!(p3.env_versions.get("dev"), Some(&2));
        assert_eq!(p3.env_versions.get("prod"), Some(&1));
    }

    #[test]
    fn delete_increments_env_version() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("A", "dev", "1").unwrap();
        store.set("B", "prod", "2").unwrap();
        let p = store.delete("A", "dev").unwrap();
        assert_eq!(p.env_versions.get("dev"), Some(&2));
        assert_eq!(p.env_versions.get("prod"), Some(&1));
    }

    #[test]
    fn env_versions_absent_from_old_payloads() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let json = r#"{"secrets":{"KEY:dev":"val"},"version":1}"#;
        let encrypted = store.encrypt(json).unwrap();
        std::fs::write(dir.path().join(".lockbox/store.enc"), &encrypted).unwrap();
        let payload = store.payload().unwrap();
        assert!(payload.env_versions.is_empty());
    }

    #[test]
    fn tombstone_absent_from_old_payloads() {
        // Simulate an old-format payload without tombstones field
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let json = r#"{"secrets":{"KEY:dev":"val"},"version":1}"#;
        let encrypted = store.encrypt(json).unwrap();
        std::fs::write(dir.path().join(".lockbox/store.enc"), &encrypted).unwrap();

        let payload = store.payload().unwrap();
        assert!(payload.tombstones.is_empty());
        assert_eq!(payload.secrets.get("KEY:dev").unwrap(), "val");
    }

    #[test]
    fn store_unicode_values() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("EMOJI", "dev", "🔐🔑✨").unwrap();
        store.set("CJK", "dev", "秘密鍵").unwrap();
        assert_eq!(
            store.get("EMOJI", "dev").unwrap(),
            Some("🔐🔑✨".to_string())
        );
        assert_eq!(store.get("CJK", "dev").unwrap(), Some("秘密鍵".to_string()));
    }
}
