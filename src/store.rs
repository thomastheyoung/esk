use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
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
}

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
        let store_path = root.join(".secrets.enc");
        let key_path = root.join(".secrets.key");

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
            };
            store.write_payload(&payload)?;
        }

        Ok(store)
    }

    /// Open an existing store (errors if key or store file is missing).
    pub fn open(root: &Path) -> Result<Self> {
        let store_path = root.join(".secrets.enc");
        let key_path = root.join(".secrets.key");

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

    /// Decrypt and return the full payload.
    pub fn payload(&self) -> Result<StorePayload> {
        let ciphertext = std::fs::read_to_string(&self.store_path)
            .with_context(|| format!("failed to read {}", self.store_path.display()))?;
        let ciphertext = ciphertext.trim();
        if ciphertext.is_empty() {
            return Ok(StorePayload {
                secrets: BTreeMap::new(),
                version: 0,
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

    /// Set a secret, incrementing the version.
    pub fn set(&self, key: &str, env: &str, value: &str) -> Result<StorePayload> {
        let mut payload = self.payload()?;
        let composite = format!("{key}:{env}");
        payload.secrets.insert(composite, value.to_string());
        payload.version += 1;
        self.write_payload(&payload)?;
        Ok(payload)
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
            bail!("invalid nonce length: expected 12, got {}", nonce_bytes.len());
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
