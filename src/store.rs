use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use hkdf::Hkdf;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

/// Validate that a secret key matches `[A-Za-z_][A-Za-z0-9_]*`.
/// Prevents shell injection, format corruption, and target compatibility issues.
pub fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("invalid secret key '': must match [A-Za-z_][A-Za-z0-9_]*");
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        bail!("invalid secret key '{key}': must match [A-Za-z_][A-Za-z0-9_]*");
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            bail!("invalid secret key '{key}': must match [A-Za-z_][A-Za-z0-9_]*");
        }
    }
    Ok(())
}

/// Validate a config identifier (environment, project, app name).
///
/// Must match `[a-zA-Z][a-zA-Z0-9_-]*`, max 64 chars. Blocks path separators,
/// spaces, colons, newlines, and other characters that could cause injection
/// when interpolated into file paths, YAML, or CLI arguments.
pub fn validate_identifier(name: &str, label: &str) -> Result<()> {
    if name.is_empty() {
        bail!("invalid {label} '': must not be empty");
    }
    if name.len() > 64 {
        bail!(
            "invalid {label} '{}...': exceeds 64 character limit",
            &name[..32]
        );
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() {
        bail!(
            "invalid {label} '{name}': must start with a letter and match [a-zA-Z][a-zA-Z0-9_-]*"
        );
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' && c != '-' {
            bail!("invalid {label} '{name}': must match [a-zA-Z][a-zA-Z0-9_-]*");
        }
    }
    Ok(())
}

/// Validate an environment name.
pub fn validate_environment(name: &str) -> Result<()> {
    validate_identifier(name, "environment")
}

/// Validate a project name.
pub fn validate_project(name: &str) -> Result<()> {
    validate_identifier(name, "project")
}

/// Validate an app name.
pub fn validate_app(name: &str) -> Result<()> {
    validate_identifier(name, "app")
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StorePayload {
    pub secrets: BTreeMap<String, String>,
    pub version: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tombstones: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_versions: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_last_changed_at: BTreeMap<String, String>,
}

impl StorePayload {
    /// Returns the effective version for a given environment.
    ///
    /// If the environment has a per-env version, returns that. If no per-env
    /// versions exist at all (pre-env-versioning store), falls back to the
    /// global version. Otherwise the environment is unknown and returns 0.
    pub fn env_version(&self, env: &str) -> u64 {
        match self.env_versions.get(env).copied() {
            Some(v) => v,
            None if self.env_versions.is_empty() => self.version,
            None => 0,
        }
    }

    /// Returns the RFC3339 timestamp for when the environment's version
    /// last changed, if known.
    pub fn env_last_changed_at(&self, env: &str) -> Option<&str> {
        self.env_last_changed_at.get(env).map(String::as_str)
    }

    /// Extract bare-key secrets for a specific environment.
    /// Returns the filtered secrets (with `:env` suffix stripped) and the resolved version.
    /// Returns `None` if no secrets match the given environment.
    pub fn env_secrets(&self, env: &str) -> Option<(BTreeMap<String, String>, u64)> {
        let suffix = format!(":{env}");
        let env_secrets: BTreeMap<String, String> = self
            .secrets
            .iter()
            .filter_map(|(k, v)| {
                k.strip_suffix(&suffix)
                    .map(|bare| (bare.to_string(), v.clone()))
            })
            .collect();

        if env_secrets.is_empty() {
            return None;
        }

        let version = self.env_version(env);

        Some((env_secrets, version))
    }
}

impl std::fmt::Debug for StorePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorePayload")
            .field("secrets", &format_args!("<{} entries>", self.secrets.len()))
            .field("version", &self.version)
            .field("tombstones", &self.tombstones)
            .field("env_versions", &self.env_versions)
            .field("env_last_changed_at", &self.env_last_changed_at)
            .finish()
    }
}

pub(crate) enum KeyProvider {
    File {
        path: PathBuf,
    },
    #[cfg_attr(not(feature = "keychain"), allow(dead_code))]
    Keychain {
        service: String,
        account: String,
    },
}

impl KeyProvider {
    pub(crate) fn from_marker(esk_dir: &Path) -> Result<Self> {
        let marker = esk_dir.join("key-provider");
        let provider = if marker.is_file() {
            std::fs::read_to_string(&marker)
                .with_context(|| format!("failed to read {}", marker.display()))?
                .trim()
                .to_string()
        } else {
            "file".to_string()
        };
        match provider.as_str() {
            "file" => Ok(Self::File {
                path: esk_dir.join("store.key"),
            }),
            "keychain" => {
                let root = esk_dir.parent().context("esk dir has no parent")?;
                let canonical = std::fs::canonicalize(root)
                    .with_context(|| format!("failed to canonicalize {}", root.display()))?;
                Ok(Self::Keychain {
                    service: "esk".to_string(),
                    account: canonical.to_string_lossy().into_owned(),
                })
            }
            other => bail!("unknown key provider in .esk/key-provider: {other}"),
        }
    }

    fn exists(&self) -> bool {
        match self {
            Self::File { path } => path.is_file(),
            #[cfg(feature = "keychain")]
            Self::Keychain { service, account } => {
                let entry = keyring::Entry::new(service, account);
                match entry {
                    Ok(e) => e.get_secret().is_ok(),
                    Err(_) => false,
                }
            }
            #[cfg(not(feature = "keychain"))]
            Self::Keychain { .. } => false,
        }
    }

    pub(crate) fn load(&self) -> Result<Zeroizing<Vec<u8>>> {
        match self {
            Self::File { path } => Self::read_key_file(path),
            #[cfg(feature = "keychain")]
            Self::Keychain { service, account } => {
                let entry = keyring::Entry::new(service, account)
                    .map_err(|e| anyhow::anyhow!("failed to access OS keychain: {e}"))?;
                let hex_str = entry.get_password().map_err(|e| match e {
                    keyring::Error::NoEntry => anyhow::anyhow!(
                        "encryption key not found in OS keychain for {account}. Run 'esk init --keychain' to set up."
                    ),
                    keyring::Error::PlatformFailure(_) | keyring::Error::NoStorageAccess(_) => {
                        anyhow::anyhow!(
                            "OS keychain is not available (headless or unsupported platform). Use file-based key storage instead."
                        )
                    }
                    _ => anyhow::anyhow!("failed to read key from OS keychain: {e}"),
                })?;
                let key = Zeroizing::new(
                    hex::decode(hex_str.trim()).context("invalid key hex from keychain")?,
                );
                if key.len() != 32 {
                    bail!(
                        "invalid key length from keychain: expected 32 bytes, got {}",
                        key.len()
                    );
                }
                Ok(key)
            }
            #[cfg(not(feature = "keychain"))]
            Self::Keychain { .. } => {
                bail!("keychain support is not available in this build. Use file-based key storage instead.")
            }
        }
    }

    fn create(&self) -> Result<Zeroizing<Vec<u8>>> {
        let key = Self::generate_key();
        self.store(&key)?;
        Ok(key)
    }

    pub(crate) fn store(&self, key: &[u8]) -> Result<()> {
        match self {
            Self::File { path } => Self::write_key_file(path, key),
            #[cfg(feature = "keychain")]
            Self::Keychain { service, account } => {
                let entry = keyring::Entry::new(service, account)
                    .map_err(|e| anyhow::anyhow!("failed to access OS keychain: {e}"))?;
                entry.set_password(&hex::encode(key)).map_err(|e| match e {
                    keyring::Error::PlatformFailure(_) | keyring::Error::NoStorageAccess(_) => {
                        anyhow::anyhow!(
                            "OS keychain is not available (headless or unsupported platform). Use file-based key storage instead."
                        )
                    }
                    _ => anyhow::anyhow!("failed to store key in OS keychain: {e}"),
                })?;
                Ok(())
            }
            #[cfg(not(feature = "keychain"))]
            Self::Keychain { .. } => {
                bail!("keychain support is not available in this build. Use file-based key storage instead.")
            }
        }
    }

    fn generate_key() -> Zeroizing<Vec<u8>> {
        let mut key = Zeroizing::new(vec![0u8; 32]);
        rand::rng().fill_bytes(&mut key);
        key
    }

    fn read_key_file(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
        let hex_str = Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("failed to read key from {}", path.display()))?,
        );
        let key = Zeroizing::new(hex::decode(hex_str.trim()).context("invalid key hex")?);
        if key.len() != 32 {
            bail!("invalid key length: expected 32 bytes, got {}", key.len());
        }
        Ok(key)
    }

    fn write_key_file(path: &Path, key: &[u8]) -> Result<()> {
        let dir = path.parent().context("key path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        let hex_key = Zeroizing::new(hex::encode(key));
        std::fs::write(tmp.path(), hex_key.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
        }
        tmp.persist(path)
            .with_context(|| format!("failed to persist key to {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn write_marker(esk_dir: &Path, value: &str) -> Result<()> {
        let marker = esk_dir.join("key-provider");
        std::fs::write(&marker, value)
            .with_context(|| format!("failed to write {}", marker.display()))?;
        Ok(())
    }
}

pub struct SecretStore {
    key: Zeroizing<Vec<u8>>,
    store_path: PathBuf,
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("store_path", &self.store_path)
            .finish_non_exhaustive()
    }
}

impl SecretStore {
    /// Load existing store or create a new empty one.
    pub fn load_or_create(root: &Path) -> Result<Self> {
        Self::load_or_create_with_provider(root, None)
    }

    /// Load existing store or create a new one, optionally forcing a specific key provider.
    /// When `provider_override` is `Some`, writes the marker file and uses that provider.
    pub(crate) fn load_or_create_with_provider(
        root: &Path,
        provider_override: Option<&str>,
    ) -> Result<Self> {
        let esk_dir = root.join(".esk");
        if !esk_dir.is_dir() {
            std::fs::create_dir_all(&esk_dir)
                .with_context(|| format!("failed to create {}", esk_dir.display()))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&esk_dir, std::fs::Permissions::from_mode(0o700))?;
        }

        if let Some(prov) = provider_override {
            KeyProvider::write_marker(&esk_dir, prov)?;
        }

        let provider = KeyProvider::from_marker(&esk_dir)?;
        let store_path = esk_dir.join("store.enc");

        let key = if provider.exists() {
            provider.load()?
        } else {
            provider.create()?
        };

        let store = Self { key, store_path };

        // Create empty store file if it doesn't exist
        if !store.store_path.is_file() {
            let payload = StorePayload {
                secrets: BTreeMap::new(),
                version: 0,
                tombstones: BTreeMap::new(),
                env_versions: BTreeMap::new(),
                env_last_changed_at: BTreeMap::new(),
            };
            store.write_payload(&payload)?;
        }

        Ok(store)
    }

    /// Open an existing store (errors if key or store file is missing).
    pub fn open(root: &Path) -> Result<Self> {
        let esk_dir = root.join(".esk");
        let store_path = esk_dir.join("store.enc");

        let provider = KeyProvider::from_marker(&esk_dir)?;

        if !provider.exists() {
            bail!("encryption key not found. Run `esk init` first.");
        }
        if !store_path.is_file() {
            bail!(
                "encrypted store not found at {}. Run `esk init` first.",
                store_path.display()
            );
        }

        let key = provider.load()?;
        Ok(Self { key, store_path })
    }

    fn lock_path(&self) -> PathBuf {
        self.store_path
            .parent()
            .expect("store_path has no parent")
            .join("lock")
    }

    /// Acquire an exclusive file lock on `.esk/lock`, run the closure, then release.
    fn with_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R>,
    {
        let lock_path = self.lock_path();
        if !lock_path.exists() {
            std::fs::File::create(&lock_path)
                .with_context(|| format!("failed to create lock file {}", lock_path.display()))?;
        }
        let file = std::fs::File::open(&lock_path)
            .with_context(|| format!("failed to open {} for locking", lock_path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("failed to acquire lock on {}", lock_path.display()))?;
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
                env_last_changed_at: BTreeMap::new(),
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
        validate_key(key)?;
        if value.contains('\0') {
            bail!("secret value for '{key}' contains null bytes");
        }
        self.with_lock(|| {
            let mut payload = self.payload()?;
            let composite = format!("{key}:{env}");
            payload.secrets.insert(composite.clone(), value.to_string());
            payload.tombstones.remove(&composite);
            payload.version += 1;
            let env_v = payload.env_versions.entry(env.to_string()).or_insert(0);
            *env_v += 1;
            payload
                .env_last_changed_at
                .insert(env.to_string(), chrono::Utc::now().to_rfc3339());
            self.write_payload(&payload)?;
            Ok(payload)
        })
    }

    /// Delete a secret, adding a tombstone. Acquires exclusive lock.
    pub fn delete(&self, key: &str, env: &str) -> Result<StorePayload> {
        validate_key(key)?;
        self.with_lock(|| {
            let mut payload = self.payload()?;
            let composite = format!("{key}:{env}");
            if payload.secrets.remove(&composite).is_none() {
                bail!("secret '{key}' has no value for environment '{env}'");
            }
            payload.version += 1;
            let env_v = payload.env_versions.entry(env.to_string()).or_insert(0);
            *env_v += 1;
            payload
                .env_last_changed_at
                .insert(env.to_string(), chrono::Utc::now().to_rfc3339());
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
    pub(crate) fn write_payload(&self, payload: &StorePayload) -> Result<()> {
        let json = Zeroizing::new(serde_json::to_string(payload)?);
        let encrypted = self.encrypt(&json)?;

        let dir = self
            .store_path
            .parent()
            .context("store path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), &encrypted)?;
        // Restrict permissions before persisting so the file is never world-readable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
        }
        tmp.persist(&self.store_path)
            .with_context(|| format!("failed to persist store to {}", self.store_path.display()))?;
        Ok(())
    }

    /// Expose the master key for domain-specific derivation.
    pub(crate) fn master_key(&self) -> &[u8] {
        &self.key
    }

    /// Encrypt arbitrary plaintext into nonce:ciphertext:tag hex format.
    pub(crate) fn encrypt(&self, plaintext: &str) -> Result<String> {
        encrypt_with_key(&self.key, plaintext)
    }

    /// Decrypt ciphertext (nonce:ciphertext:tag hex format) into a StorePayload.
    pub(crate) fn decrypt(&self, encoded: &str) -> Result<StorePayload> {
        let json = Zeroizing::new(decrypt_with_key(&self.key, encoded)?);
        serde_json::from_str(&json).context("decrypted payload is not valid JSON")
    }
}

/// Encrypt plaintext with the given key. Returns nonce:ciphertext:tag hex.
pub(crate) fn encrypt_with_key(key: &[u8], plaintext: &str) -> Result<String> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;

    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
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

/// Decrypt nonce:ciphertext:tag hex with the given key. Returns plaintext string.
pub(crate) fn decrypt_with_key(key: &[u8], encoded: &str) -> Result<String> {
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

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Recombine ciphertext + tag for aes-gcm
    let mut combined = ct_bytes;
    combined.extend_from_slice(&tag_bytes);

    let plaintext = cipher
        .decrypt(nonce, combined.as_ref())
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong key or corrupted store"))?;

    String::from_utf8(plaintext).context("decrypted payload is not valid UTF-8")
}

/// Derive a 32-byte domain-specific key from the master key via HKDF-SHA256.
///
/// Uses `None` for salt per RFC 5869 §3.1: when IKM is already uniformly random
/// (32 bytes from CSPRNG), a salt is not required. Domain separation is handled
/// by the `info` parameter. A fixed app salt would be a breaking change for
/// existing encrypted remotes with no meaningful security gain.
pub(crate) fn derive_key(master: &[u8], domain: &[u8]) -> Zeroizing<Vec<u8>> {
    let hk = Hkdf::<Sha256>::new(None, master);
    let mut out = Zeroizing::new(vec![0u8; 32]);
    hk.expand(domain, &mut out)
        .expect("32 bytes is valid HKDF-SHA256 output");
    out
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
        assert!(dir.path().join(".esk/store.key").is_file());
        assert!(dir.path().join(".esk/store.enc").is_file());
        let payload = store.payload().unwrap();
        assert!(payload.secrets.is_empty());
        assert_eq!(payload.version, 0);
    }

    #[test]
    fn load_or_create_existing() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        let key_before = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();

        let store2 = SecretStore::load_or_create(dir.path()).unwrap();
        let key_after = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();
        assert_eq!(key_before, key_after);
        assert_eq!(store2.get("KEY", "dev").unwrap(), Some("val".to_string()));
    }

    #[test]
    fn load_or_create_key_exists_no_store() {
        let dir = tmp_root();
        // Create key only
        SecretStore::load_or_create(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(".esk/store.enc")).unwrap();

        let store = SecretStore::load_or_create(dir.path()).unwrap();
        assert!(dir.path().join(".esk/store.enc").is_file());
        let payload = store.payload().unwrap();
        assert_eq!(payload.version, 0);
    }

    #[test]
    fn open_missing_key() {
        let dir = tmp_root();
        // Create .esk dir so from_marker can run, but no key file
        std::fs::create_dir_all(dir.path().join(".esk")).unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("encryption key not found"));
    }

    #[test]
    fn open_missing_store() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(".esk/store.enc")).unwrap();
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
        std::fs::write(dir.path().join(".esk/store.enc"), "").unwrap();
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
        let metadata = std::fs::metadata(dir.path().join(".esk/store.key")).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn key_is_32_bytes() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        let hex_str = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();
        let key_bytes = hex::decode(hex_str.trim()).unwrap();
        assert_eq!(key_bytes.len(), 32);
    }

    #[test]
    fn key_hex_roundtrip() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        let hex_str = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();
        let key_bytes = hex::decode(hex_str.trim()).unwrap();
        assert_eq!(hex::encode(&key_bytes), hex_str.trim());
    }

    #[test]
    fn write_payload_atomic() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        assert!(dir.path().join(".esk/store.enc").is_file());
        // No temp files left behind
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
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
        std::fs::write(dir.path().join(".esk/store.key"), "not_valid_hex_zzz").unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid key hex"));
    }

    #[test]
    fn empty_key_file() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        std::fs::write(dir.path().join(".esk/store.key"), "").unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid key length"));
    }

    #[test]
    fn delete_removes_secret() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        let payload = store.delete("KEY", "dev").unwrap();
        assert_eq!(payload.version, 2);
        assert!(!payload.secrets.contains_key("KEY:dev"));
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
        assert!(!payload.secrets.contains_key("A:dev"));
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
    fn set_and_delete_update_env_last_changed_at() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();

        let p1 = store.set("A", "dev", "1").unwrap();
        assert!(p1.env_last_changed_at("dev").is_some());

        let p2 = store.set("B", "prod", "2").unwrap();
        assert!(p2.env_last_changed_at("dev").is_some());
        assert!(p2.env_last_changed_at("prod").is_some());

        let p3 = store.delete("A", "dev").unwrap();
        assert!(p3.env_last_changed_at("dev").is_some());
        assert!(p3.env_last_changed_at("prod").is_some());
    }

    #[test]
    fn env_versions_absent_from_old_payloads() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let json = r#"{"secrets":{"KEY:dev":"val"},"version":1}"#;
        let encrypted = store.encrypt(json).unwrap();
        std::fs::write(dir.path().join(".esk/store.enc"), &encrypted).unwrap();
        let payload = store.payload().unwrap();
        assert!(payload.env_versions.is_empty());
    }

    #[test]
    fn env_last_changed_at_absent_from_old_payloads() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let json = r#"{"secrets":{"KEY:dev":"val"},"version":1}"#;
        let encrypted = store.encrypt(json).unwrap();
        std::fs::write(dir.path().join(".esk/store.enc"), &encrypted).unwrap();
        let payload = store.payload().unwrap();
        assert!(payload.env_last_changed_at.is_empty());
    }

    #[test]
    fn tombstone_absent_from_old_payloads() {
        // Simulate an old-format payload without tombstones field
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let json = r#"{"secrets":{"KEY:dev":"val"},"version":1}"#;
        let encrypted = store.encrypt(json).unwrap();
        std::fs::write(dir.path().join(".esk/store.enc"), &encrypted).unwrap();

        let payload = store.payload().unwrap();
        assert!(payload.tombstones.is_empty());
        assert_eq!(payload.secrets.get("KEY:dev").unwrap(), "val");
    }

    #[test]
    fn validate_key_valid() {
        assert!(validate_key("API_KEY").is_ok());
        assert!(validate_key("_PRIVATE").is_ok());
        assert!(validate_key("a").is_ok());
        assert!(validate_key("A123").is_ok());
        assert!(validate_key("my_secret_key_42").is_ok());
    }

    #[test]
    fn validate_key_invalid() {
        assert!(validate_key("").is_err());
        assert!(validate_key("123ABC").is_err());
        assert!(validate_key("KEY-NAME").is_err());
        assert!(validate_key("KEY.NAME").is_err());
        assert!(validate_key("KEY NAME").is_err());
        assert!(validate_key("KEY=VAL").is_err());
        assert!(validate_key("$KEY").is_err());
    }

    #[test]
    fn set_rejects_invalid_key() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.set("invalid-key", "dev", "val").unwrap_err();
        assert!(err.to_string().contains("invalid secret key"));
    }

    #[test]
    fn delete_rejects_invalid_key() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("VALID_KEY", "dev", "val").unwrap();
        let err = store.delete("invalid-key", "dev").unwrap_err();
        assert!(err.to_string().contains("invalid secret key"));
    }

    // --- Phase 2a: identifier validation tests ---

    #[test]
    fn validate_identifier_valid() {
        assert!(validate_identifier("dev", "env").is_ok());
        assert!(validate_identifier("prod", "env").is_ok());
        assert!(validate_identifier("staging_v2", "env").is_ok());
        assert!(validate_identifier("my-app", "app").is_ok());
        assert!(validate_identifier("MyProject", "project").is_ok());
    }

    #[test]
    fn validate_identifier_empty() {
        let err = validate_identifier("", "env").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_identifier_path_separator() {
        let err = validate_identifier("../escape", "env").unwrap_err();
        assert!(err.to_string().contains("must start with a letter"));
    }

    #[test]
    fn validate_identifier_colon() {
        let err = validate_identifier("key:val", "env").unwrap_err();
        assert!(err.to_string().contains("must match"));
    }

    #[test]
    fn validate_identifier_newline() {
        let err = validate_identifier("dev\ninjection", "env").unwrap_err();
        assert!(err.to_string().contains("must match"));
    }

    #[test]
    fn validate_identifier_space() {
        let err = validate_identifier("my app", "env").unwrap_err();
        assert!(err.to_string().contains("must match"));
    }

    #[test]
    fn validate_identifier_starts_with_number() {
        let err = validate_identifier("123abc", "env").unwrap_err();
        assert!(err.to_string().contains("must start with a letter"));
    }

    #[test]
    fn validate_identifier_too_long() {
        let long = "a".repeat(65);
        let err = validate_identifier(&long, "env").unwrap_err();
        assert!(err.to_string().contains("exceeds 64"));
    }

    // --- Phase 4a: debug redaction tests ---

    #[test]
    fn store_payload_debug_redacts_secrets() {
        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:dev".to_string(), "super_secret_value".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };
        let debug = format!("{payload:?}");
        assert!(!debug.contains("super_secret_value"));
        assert!(debug.contains("1 entries"));
    }

    #[test]
    fn secret_store_debug_redacts_key() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let debug = format!("{store:?}");
        assert!(!debug.contains(&hex::encode(&store.key)));
        assert!(debug.contains("store_path"));
    }

    // --- Phase 5a: directory permissions ---

    #[test]
    #[cfg(unix)]
    fn esk_dir_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        let metadata = std::fs::metadata(dir.path().join(".esk")).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    // --- Phase 5b: store.enc permissions ---

    #[test]
    #[cfg(unix)]
    fn store_enc_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap();
        let metadata = std::fs::metadata(dir.path().join(".esk/store.enc")).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // --- Phase 5c: key length validation ---

    #[test]
    fn key_load_rejects_short_key() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        // Write a 16-byte key (32 hex chars for 16 bytes)
        let short_key = hex::encode([0u8; 16]);
        std::fs::write(dir.path().join(".esk/store.key"), &short_key).unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid key length"));
        assert!(err.to_string().contains("expected 32 bytes, got 16"));
    }

    #[test]
    fn key_load_rejects_empty() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        std::fs::write(dir.path().join(".esk/store.key"), "").unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid key length"));
    }

    // --- Phase 6a: null byte rejection ---

    #[test]
    fn set_rejects_null_bytes() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let err = store.set("KEY", "dev", "val\0ue").unwrap_err();
        assert!(err.to_string().contains("contains null bytes"));
    }

    #[test]
    fn set_accepts_newlines() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "line1\nline2").unwrap();
        assert_eq!(
            store.get("KEY", "dev").unwrap(),
            Some("line1\nline2".to_string())
        );
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

    #[test]
    fn env_version_returns_per_env_version() {
        let mut payload = StorePayload {
            secrets: BTreeMap::new(),
            version: 10,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };
        payload.env_versions.insert("dev".to_string(), 3);
        assert_eq!(payload.env_version("dev"), 3);
    }

    #[test]
    fn env_version_falls_back_to_global_when_no_env_versions() {
        let payload = StorePayload {
            secrets: BTreeMap::new(),
            version: 7,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };
        assert_eq!(payload.env_version("dev"), 7);
    }

    #[test]
    fn env_version_returns_zero_for_unknown_env() {
        let mut payload = StorePayload {
            secrets: BTreeMap::new(),
            version: 10,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };
        payload.env_versions.insert("dev".to_string(), 3);
        assert_eq!(payload.env_version("prod"), 0);
    }
}
