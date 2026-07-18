use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use hkdf::Hkdf;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

/// AES-256 key length in bytes.
const KEY_LEN: usize = 32;
/// AES-GCM nonce length in bytes.
const NONCE_LEN: usize = 12;
/// AES-GCM authentication tag length in bytes.
const TAG_LEN: usize = 16;
const STORE_FORMAT_V2: &str = "v2";
const STORE_AAD_V2: &[u8] = b"esk-store:v2";
const STORE_VERSION_FILE: &str = "store.version";
const ROTATION_JOURNAL_FILE: &str = "key-rotation.json";
/// Environment variable containing the store's master encryption key.
///
/// This is crate-visible so every subprocess boundary can explicitly remove
/// the key before spawning an external command.
pub(crate) const STORE_KEY_ENV: &str = "ESK_STORE_KEY";

/// Validate that a secret key matches `[A-Za-z_][A-Za-z0-9_]*`.
/// Prevents shell injection, format corruption, and target compatibility issues.
pub fn validate_key(key: &str) -> Result<()> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        bail!("invalid secret key '': must match [A-Za-z_][A-Za-z0-9_]*");
    };
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
pub(crate) fn validate_identifier(name: &str, label: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("invalid {label} '': must not be empty");
    };
    if name.len() > 64 {
        let truncated: String = name.chars().take(32).collect();
        bail!("invalid {label} '{truncated}...': exceeds 64 character limit");
    }
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

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct StorePayload {
    pub secrets: BTreeMap<String, String>,
    /// Monotonic high-water mark incremented on every set/delete across all environments.
    ///
    /// NOT used for reconcile decisions (env_versions handles that). Serves as:
    /// - Tombstone version base (tombstones carry this value for cross-env consistency)
    /// - Backward-compat fallback for pre-env-versioning stores (via `env_version()`)
    /// - Monotonic ceiling in reconcile output (`.max(local.version)`)
    pub version: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tombstones: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_versions: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_last_changed_at: BTreeMap<String, String>,
}

const ROTATION_JOURNAL_VERSION: u8 = 1;
const ROTATION_STAGE_ID_BYTES: usize = 16;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RotationJournal {
    version: u8,
    stage_id: String,
    phase: RotationPhase,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RotationPhase {
    Prepared,
    StorePromoted,
    ProviderPromoted,
}

struct RotationStage {
    candidate_store: PathBuf,
    staged_key: KeyProvider,
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

    /// Build a per-env StorePayload with bare keys for syncing to remotes.
    /// Strips the `:{env}` suffix from secret keys and includes env-specific
    /// version and timestamp. Returns a payload with empty tombstones and env_versions.
    #[must_use]
    pub fn for_env(&self, env: &str) -> StorePayload {
        let suffix = format!(":{env}");
        let bare: BTreeMap<String, String> = self
            .secrets
            .iter()
            .filter_map(|(k, v)| {
                k.strip_suffix(&suffix)
                    .map(|bare| (bare.to_string(), v.clone()))
            })
            .collect();
        let version = self.env_version(env);
        let mut env_last_changed_at = BTreeMap::new();
        if let Some(ts) = self.env_last_changed_at(env) {
            env_last_changed_at.insert(env.to_string(), ts.to_string());
        }
        StorePayload {
            secrets: bare,
            version,
            env_last_changed_at,
            ..Default::default()
        }
    }

    /// Convert bare keys back to composite keys (`KEY` → `KEY:env`).
    pub fn bare_to_composite(
        secrets: &BTreeMap<String, String>,
        env: &str,
    ) -> BTreeMap<String, String> {
        secrets
            .iter()
            .map(|(k, v)| (format!("{k}:{env}"), v.clone()))
            .collect()
    }
}

impl StorePayload {
    /// Prune tombstones that all configured remotes have acknowledged.
    ///
    /// For each tombstone, extracts the env from the composite key (`KEY:env`),
    /// looks up the minimum successfully-pushed version across all remotes for
    /// that env, and removes the tombstone if its version <= that minimum.
    ///
    /// Returns the number of pruned entries.
    pub fn prune_tombstones(
        &mut self,
        sync_index: &crate::sync_tracker::SyncIndex,
        remote_names: &[&str],
    ) -> usize {
        if self.tombstones.is_empty() || remote_names.is_empty() {
            return 0;
        }

        // Pre-compute min push version per env to avoid borrow conflict with retain
        let envs: std::collections::BTreeSet<&str> = self
            .tombstones
            .keys()
            .filter_map(|k| k.rsplit_once(':').map(|(_, env)| env))
            .collect();

        let min_versions: BTreeMap<String, Option<u64>> = envs
            .into_iter()
            .map(|env| {
                (
                    env.to_string(),
                    sync_index.min_successful_push_version(env, remote_names),
                )
            })
            .collect();

        let before = self.tombstones.len();
        self.tombstones.retain(|key, tomb_version| {
            let env = key.rsplit_once(':').map_or("", |(_, e)| e);
            match min_versions.get(env).copied().flatten() {
                Some(min_v) => *tomb_version > min_v, // keep if not yet acknowledged
                None => true,                         // keep if we can't confirm all remotes
            }
        });
        before - self.tombstones.len()
    }
}

impl std::fmt::Debug for StorePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorePayload")
            .field("secrets", &format_args!("<{} entries>", self.secrets.len()))
            .field("version", &self.version)
            .field(
                "tombstones",
                &format_args!("<{} entries>", self.tombstones.len()),
            )
            .field("env_versions", &self.env_versions)
            .field("env_last_changed_at", &self.env_last_changed_at)
            .finish()
    }
}

pub(crate) enum KeyProvider {
    Environment {
        key: Zeroizing<Vec<u8>>,
    },
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
    /// Select the environment key when present, otherwise use the configured
    /// persistent provider from `.esk/key-provider`.
    ///
    /// Presence, rather than successful parsing, determines precedence. This
    /// prevents a malformed `ESK_STORE_KEY` from silently falling back to a
    /// different key source and producing a confusing decryption failure.
    pub(crate) fn from_environment_or_marker(esk_dir: &Path) -> Result<Self> {
        let value = std::env::var_os(STORE_KEY_ENV)
            .map(|value| {
                value
                    .into_string()
                    .map(Zeroizing::new)
                    .map_err(|_| anyhow::anyhow!("{STORE_KEY_ENV} is set but is not valid UTF-8"))
            })
            .transpose()?;
        Self::from_environment_value_or_marker(esk_dir, value.as_ref().map(|value| value.as_str()))
    }

    fn from_environment_value_or_marker(
        esk_dir: &Path,
        environment_value: Option<&str>,
    ) -> Result<Self> {
        match environment_value {
            Some(value) => Self::from_environment_value(value),
            None => Self::from_marker(esk_dir),
        }
    }

    fn from_environment_value(value: &str) -> Result<Self> {
        Ok(Self::Environment {
            key: Self::decode_key(value.trim(), STORE_KEY_ENV)?,
        })
    }

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
            Self::Environment { .. } => true,
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
            Self::Environment { key } => Ok(key.clone()),
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
                if key.len() != KEY_LEN {
                    bail!(
                        "invalid key length from keychain: expected {KEY_LEN} bytes, got {}",
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
            Self::Environment { .. } => bail!(
                "cannot write a new encryption key to {STORE_KEY_ENV}; unset {STORE_KEY_ENV} and use the configured file or OS-keychain provider"
            ),
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
        let mut key = Zeroizing::new(vec![0u8; KEY_LEN]);
        rand::rng().fill_bytes(&mut key);
        key
    }

    fn read_key_file(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
        let hex_str = Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("failed to read key from {}", path.display()))?,
        );
        let key = Self::decode_key(hex_str.trim(), "key")?;
        Ok(key)
    }

    fn decode_key(value: &str, source: &str) -> Result<Zeroizing<Vec<u8>>> {
        let key =
            Zeroizing::new(hex::decode(value).with_context(|| format!("invalid {source} hex"))?);
        if key.len() != KEY_LEN {
            bail!(
                "invalid {source} length: expected {KEY_LEN} bytes, got {}",
                key.len()
            );
        }
        Ok(key)
    }

    fn write_key_file(path: &Path, key: &[u8]) -> Result<()> {
        let dir = path.parent().context("key path has no parent")?;
        let mut tmp = NamedTempFile::new_in(dir)?;
        let hex_key = Zeroizing::new(hex::encode(key));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
        }
        tmp.as_file_mut().write_all(hex_key.as_bytes())?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(path)
            .with_context(|| format!("failed to persist key to {}", path.display()))?;
        sync_directory(dir)?;
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
    version_path: PathBuf,
}

/// Acquire the project-wide mutation lock shared by the encrypted store and
/// tracker files. The returned file holds the lock until dropped.
pub(crate) fn acquire_project_lock(root: &Path) -> Result<File> {
    let esk_dir = root.join(".esk");
    let lock_path = esk_dir.join("lock");
    if !lock_path.exists() {
        File::create(&lock_path)
            .with_context(|| format!("failed to create lock file {}", lock_path.display()))?;
    }
    let file = File::open(&lock_path)
        .with_context(|| format!("failed to open {} for locking", lock_path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("failed to acquire lock on {}", lock_path.display()))?;
    Ok(file)
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

        let provider = match provider_override {
            Some(prov) => {
                // Explicit initialization choices must be deterministic even
                // when a CI environment happens to export ESK_STORE_KEY.
                KeyProvider::write_marker(&esk_dir, prov)?;
                KeyProvider::from_marker(&esk_dir)?
            }
            None => KeyProvider::from_environment_or_marker(&esk_dir)?,
        };
        if rotation_journal_exists(&esk_dir)? {
            recover_pending_rotation_with_durable_provider(&esk_dir, &provider)?;
        }
        let store_path = esk_dir.join("store.enc");

        let key = if provider.exists() {
            provider.load()?
        } else {
            provider.create()?
        };

        let version_path = esk_dir.join(STORE_VERSION_FILE);
        let store = Self {
            key,
            store_path,
            version_path,
        };

        // Create empty store file if it doesn't exist
        if !store.store_path.is_file() {
            store.write_payload(&StorePayload::default())?;
        }

        Ok(store)
    }

    /// Open an existing store (errors if key or store file is missing).
    pub fn open(root: &Path) -> Result<Self> {
        let esk_dir = root.join(".esk");
        let provider = KeyProvider::from_environment_or_marker(&esk_dir)?;
        Self::open_with_provider(root, &provider)
    }

    fn open_with_provider(root: &Path, provider: &KeyProvider) -> Result<Self> {
        let esk_dir = root.join(".esk");
        let store_path = esk_dir.join("store.enc");

        if rotation_journal_exists(&esk_dir)? {
            recover_pending_rotation_with_durable_provider(&esk_dir, provider)?;
        }

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
        let version_path = esk_dir.join(STORE_VERSION_FILE);
        Ok(Self {
            key,
            store_path,
            version_path,
        })
    }

    /// Acquire an exclusive file lock on `.esk/lock`, run the closure, then release.
    fn with_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R>,
    {
        let root = self
            .store_path
            .parent()
            .and_then(Path::parent)
            .context("store path has no project root")?;
        let file = acquire_project_lock(root)?;
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
            let payload = StorePayload::default();
            self.check_rollback(&payload)?;
            return Ok(payload);
        }
        let payload = self.decrypt(ciphertext)?;
        self.check_rollback(&payload)?;
        Ok(payload)
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

    /// Set multiple values for one environment in a single locked transaction.
    pub fn set_many(&self, env: &str, values: &BTreeMap<String, String>) -> Result<StorePayload> {
        for (key, value) in values {
            validate_key(key)?;
            if value.contains('\0') {
                bail!("secret value for '{key}' contains null bytes");
            }
        }
        self.with_lock(|| {
            let mut payload = self.payload()?;
            for (key, value) in values {
                let composite = format!("{key}:{env}");
                payload.secrets.insert(composite.clone(), value.clone());
                payload.tombstones.remove(&composite);
                payload.version += 1;
                let env_v = payload.env_versions.entry(env.to_string()).or_insert(0);
                *env_v += 1;
                payload
                    .env_last_changed_at
                    .insert(env.to_string(), chrono::Utc::now().to_rfc3339());
            }
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

    /// Generate a new encryption key and re-encrypt the current store with it.
    ///
    /// Rotation is recoverable through a constrained staged state machine. The
    /// journal has no secret material or paths; all artifacts are derived from
    /// its opaque stage identifier.
    pub fn rotate_key(&self) -> Result<()> {
        self.with_lock(|| {
            let provider = KeyProvider::from_environment_or_marker(
                self.store_path
                    .parent()
                    .context("store path has no parent")?,
            )?;
            if matches!(provider, KeyProvider::Environment { .. }) {
                bail!(
                    "cannot rotate the encryption key while {STORE_KEY_ENV} is set; unset {STORE_KEY_ENV} and retry"
                );
            }

            let payload = self.payload()?;
            let json = Zeroizing::new(serde_json::to_string(&payload)?);
            let new_key = KeyProvider::generate_key();
            let encrypted = encrypt_store_with_key(&new_key, &json)?;

            let dir = self
                .store_path
                .parent()
                .context("store path has no parent")?;
            let stage_id = generate_rotation_stage_id();
            let stage = rotation_stage(dir, &provider, &stage_id)?;
            write_staged_key(&stage.staged_key, &new_key)?;
            let staged_key = match load_staged_key(&stage.staged_key) {
                Ok(key) => key,
                Err(error) => {
                    let _ = remove_staged_key(&stage.staged_key);
                    return Err(error);
                }
            };
            if let Err(error) = write_private_new_file(&stage.candidate_store, encrypted.as_bytes()) {
                let _ = remove_staged_key(&stage.staged_key);
                return Err(error);
            }
            if let Err(error) = authenticate_store_file(&stage.candidate_store, &staged_key) {
                let _ = std::fs::remove_file(&stage.candidate_store);
                let _ = sync_directory(dir);
                let _ = remove_staged_key(&stage.staged_key);
                return Err(error);
            }
            let journal_path = dir.join(ROTATION_JOURNAL_FILE);
            let journal = RotationJournal {
                version: ROTATION_JOURNAL_VERSION,
                stage_id,
                phase: RotationPhase::Prepared,
            };
            write_rotation_journal(&journal_path, &journal)?;

            std::fs::rename(&stage.candidate_store, &self.store_path).with_context(|| {
                format!("failed to promote staged store {}", stage.candidate_store.display())
            })?;
            sync_directory(dir)?;
            write_rotation_journal(
                &journal_path,
                &RotationJournal {
                    phase: RotationPhase::StorePromoted,
                    ..journal.clone()
                },
            )?;
            provider.store(&new_key)?;
            write_rotation_journal(
                &journal_path,
                &RotationJournal {
                    phase: RotationPhase::ProviderPromoted,
                    ..journal
                },
            )?;
            std::fs::remove_file(&journal_path).with_context(|| {
                format!("failed to remove {}", journal_path.display())
            })?;
            sync_directory(dir)?;
            remove_staged_key(&stage.staged_key)?;
            Ok(())
        })
    }

    /// List all secrets (returns the full BTreeMap).
    pub fn list(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.payload()?.secrets)
    }

    /// Write a payload to the store, encrypting it.
    pub(crate) fn write_payload(&self, payload: &StorePayload) -> Result<()> {
        let json = Zeroizing::new(serde_json::to_string(payload)?);
        let encrypted = self.encrypt_store(&json)?;

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
        self.write_high_water(payload.version)?;
        Ok(())
    }

    fn check_rollback(&self, payload: &StorePayload) -> Result<()> {
        let recorded = if self.version_path.is_file() {
            let text = std::fs::read_to_string(&self.version_path)
                .with_context(|| format!("failed to read {}", self.version_path.display()))?;
            text.trim().parse::<u64>().with_context(|| {
                format!(
                    "invalid store high-water mark in {}",
                    self.version_path.display()
                )
            })?
        } else {
            0
        };

        if payload.version < recorded {
            bail!(
                "store rollback detected: encrypted version {} is below local high-water mark {}",
                payload.version,
                recorded
            );
        }
        if payload.version > recorded || !self.version_path.is_file() {
            self.write_high_water(payload.version)?;
        }
        Ok(())
    }

    fn write_high_water(&self, version: u64) -> Result<()> {
        let dir = self
            .version_path
            .parent()
            .context("store version path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), version.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
        }
        tmp.persist(&self.version_path).with_context(|| {
            format!(
                "failed to persist store high-water mark to {}",
                self.version_path.display()
            )
        })?;
        Ok(())
    }

    /// Expose the master key for domain-specific derivation.
    pub(crate) fn master_key(&self) -> &[u8] {
        &self.key
    }

    /// Encrypt arbitrary plaintext into nonce:ciphertext:tag hex format.
    #[cfg(test)]
    pub(crate) fn encrypt(&self, plaintext: &str) -> Result<String> {
        encrypt_with_key(&self.key, plaintext)
    }

    /// Decrypt ciphertext (nonce:ciphertext:tag hex format) into a StorePayload.
    pub(crate) fn decrypt(&self, encoded: &str) -> Result<StorePayload> {
        let json = Zeroizing::new(if let Some(body) = encoded.strip_prefix("v2:") {
            decrypt_with_aad(&self.key, body, STORE_AAD_V2)?
        } else {
            decrypt_with_key(&self.key, encoded)?
        });
        serde_json::from_str(&json).context("decrypted payload is not valid JSON")
    }

    fn encrypt_store(&self, plaintext: &str) -> Result<String> {
        encrypt_store_with_key(&self.key, plaintext)
    }
}

fn encrypt_store_with_key(key: &[u8], plaintext: &str) -> Result<String> {
    Ok(format!(
        "{STORE_FORMAT_V2}:{}",
        encrypt_with_aad(key, plaintext, STORE_AAD_V2)?
    ))
}

fn write_rotation_journal(path: &Path, journal: &RotationJournal) -> Result<()> {
    let dir = path
        .parent()
        .context("rotation journal path has no parent")?;
    let mut tmp = NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.as_file_mut().write_all(&serde_json::to_vec(journal)?)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("failed to persist rotation journal to {}", path.display()))?;
    sync_directory(dir)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(dir: &Path) -> Result<()> {
    File::open(dir)
        .with_context(|| format!("failed to open directory {} for sync", dir.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", dir.display()))
}

#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> Result<()> {
    Ok(())
}

fn rotation_journal_exists(esk_dir: &Path) -> Result<bool> {
    let path = esk_dir.join(ROTATION_JOURNAL_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("unsafe key rotation journal at {}", path.display());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn generate_rotation_stage_id() -> String {
    let mut bytes = [0_u8; ROTATION_STAGE_ID_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn validate_rotation_stage_id(stage_id: &str) -> Result<()> {
    if stage_id.len() != ROTATION_STAGE_ID_BYTES * 2
        || !stage_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid key rotation stage identifier");
    }
    Ok(())
}

fn rotation_stage(esk_dir: &Path, provider: &KeyProvider, stage_id: &str) -> Result<RotationStage> {
    validate_rotation_stage_id(stage_id)?;
    let candidate_store = esk_dir.join(format!("rotation-{stage_id}.store"));
    let staged_key = match provider {
        KeyProvider::Environment { .. } => {
            bail!("environment key provider cannot stage a key rotation")
        }
        KeyProvider::File { .. } => KeyProvider::File {
            path: esk_dir.join(format!("rotation-{stage_id}.key")),
        },
        KeyProvider::Keychain { service, account } => KeyProvider::Keychain {
            service: service.clone(),
            account: format!("{account}:rotation:{stage_id}"),
        },
    };
    Ok(RotationStage {
        candidate_store,
        staged_key,
    })
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("unsafe {label} at {}", path.display());
    }
    Ok(())
}

fn write_private_new_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create staged artifact {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    sync_directory(
        path.parent()
            .context("staged artifact path has no parent")?,
    )?;
    Ok(())
}

fn write_staged_key(provider: &KeyProvider, key: &[u8]) -> Result<()> {
    match provider {
        KeyProvider::File { path } => write_private_new_file(path, hex::encode(key).as_bytes()),
        KeyProvider::Keychain { .. } => provider.store(key),
        KeyProvider::Environment { .. } => {
            bail!("environment key provider cannot stage a key rotation")
        }
    }
}

fn load_staged_key(provider: &KeyProvider) -> Result<Zeroizing<Vec<u8>>> {
    if let KeyProvider::File { path } = provider {
        ensure_regular_file(path, "staged rotation key")?;
    }
    provider.load()
}

fn remove_staged_key(provider: &KeyProvider) -> Result<()> {
    match provider {
        KeyProvider::File { path } => {
            ensure_regular_file(path, "staged rotation key")?;
            std::fs::remove_file(path).with_context(|| {
                format!("failed to remove staged rotation key {}", path.display())
            })?;
            sync_directory(
                path.parent()
                    .context("staged rotation key path has no parent")?,
            )
        }
        #[cfg(feature = "keychain")]
        KeyProvider::Keychain { service, account } => {
            let entry = keyring::Entry::new(service, account)
                .map_err(|error| anyhow::anyhow!("failed to access OS keychain: {error}"))?;
            entry.delete_credential().map_err(|error| {
                anyhow::anyhow!("failed to remove staged rotation key from OS keychain: {error}")
            })
        }
        #[cfg(not(feature = "keychain"))]
        KeyProvider::Keychain { .. } => Ok(()),
        KeyProvider::Environment { .. } => {
            bail!("environment key provider cannot stage a key rotation")
        }
    }
}

fn authenticate_store_file(path: &Path, key: &[u8]) -> Result<StorePayload> {
    let encoded = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read staged store {}", path.display()))?;
    let body = encoded
        .trim()
        .strip_prefix("v2:")
        .context("staged store has an unsupported format")?;
    let plaintext = Zeroizing::new(decrypt_with_aad(key, body, STORE_AAD_V2)?);
    serde_json::from_str(&plaintext).context("staged store payload is not valid JSON")
}

fn recover_pending_rotation_with_durable_provider(
    esk_dir: &Path,
    provider: &KeyProvider,
) -> Result<()> {
    // ESK_STORE_KEY is intentionally read-only. A rotation journal, however,
    // must finish updating the durable provider before the store can be
    // considered recovered. Keep the environment provider for opening the
    // recovered store, but use the marker-selected provider for this write.
    let root = esk_dir
        .parent()
        .context("rotation directory has no project root")?;
    let lock = acquire_project_lock(root)?;
    let result = if matches!(provider, KeyProvider::Environment { .. }) {
        let durable_provider = KeyProvider::from_marker(esk_dir)?;
        recover_pending_rotation(esk_dir, &durable_provider)
    } else {
        recover_pending_rotation(esk_dir, provider)
    };
    drop(lock);
    result
}

fn recover_pending_rotation(esk_dir: &Path, provider: &KeyProvider) -> Result<()> {
    let journal_path = esk_dir.join(ROTATION_JOURNAL_FILE);
    ensure_regular_file(&journal_path, "key rotation journal")?;
    let journal: RotationJournal = serde_json::from_slice(
        &std::fs::read(&journal_path)
            .with_context(|| format!("failed to read {}", journal_path.display()))?,
    )
    .with_context(|| format!("invalid key rotation journal {}", journal_path.display()))?;
    if journal.version != ROTATION_JOURNAL_VERSION {
        bail!("unsupported key rotation journal version");
    }
    let stage = rotation_stage(esk_dir, provider, &journal.stage_id)?;
    let new_key = load_staged_key(&stage.staged_key)?;
    let store_path = esk_dir.join("store.enc");
    let candidate_exists = match std::fs::symlink_metadata(&stage.candidate_store) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", stage.candidate_store.display()))
        }
    };

    match journal.phase {
        RotationPhase::Prepared if candidate_exists => {
            ensure_regular_file(&stage.candidate_store, "staged rotation store")?;
            authenticate_store_file(&stage.candidate_store, &new_key)?;
            std::fs::rename(&stage.candidate_store, &store_path).with_context(|| {
                format!(
                    "failed to promote staged store {}",
                    stage.candidate_store.display()
                )
            })?;
            sync_directory(esk_dir)?;
            write_rotation_journal(
                &journal_path,
                &RotationJournal {
                    phase: RotationPhase::StorePromoted,
                    ..journal.clone()
                },
            )?;
        }
        RotationPhase::Prepared
        | RotationPhase::StorePromoted
        | RotationPhase::ProviderPromoted
            if !candidate_exists =>
        {
            ensure_regular_file(&store_path, "promoted rotation store")?;
            authenticate_store_file(&store_path, &new_key)
                .context("key rotation journal exists but the promoted store is not recoverable")?;
        }
        RotationPhase::StorePromoted | RotationPhase::ProviderPromoted => {
            bail!("key rotation journal phase does not match staged artifacts")
        }
        RotationPhase::Prepared => bail!("key rotation journal has an invalid staged state"),
    }
    provider.store(&new_key)?;
    write_rotation_journal(
        &journal_path,
        &RotationJournal {
            phase: RotationPhase::ProviderPromoted,
            ..journal
        },
    )?;
    std::fs::remove_file(&journal_path)
        .with_context(|| format!("failed to remove {}", journal_path.display()))?;
    sync_directory(esk_dir)?;
    remove_staged_key(&stage.staged_key)?;
    Ok(())
}

/// Encrypt plaintext with the given key. Returns nonce:ciphertext:tag hex.
pub(crate) fn encrypt_with_key(key: &[u8], plaintext: &str) -> Result<String> {
    encrypt_with_aad(key, plaintext, &[])
}

fn encrypt_with_aad(key: &[u8], plaintext: &str, aad: &[u8]) -> Result<String> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            nonce,
            aes_gcm::aead::Payload {
                msg: plaintext.as_bytes(),
                aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

    // AES-GCM appends tag to ciphertext. Split for our format.
    // aes-gcm crate: ciphertext includes the TAG_LEN-byte tag at the end
    let tag_start = ciphertext.len() - TAG_LEN;
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
    decrypt_with_aad(key, encoded, &[])
}

fn decrypt_with_aad(key: &[u8], encoded: &str, aad: &[u8]) -> Result<String> {
    let parts: Vec<&str> = encoded.split(':').collect();
    if parts.len() != 3 {
        bail!("invalid store format: expected nonce:ciphertext:tag");
    }

    let nonce_bytes = hex::decode(parts[0]).context("invalid nonce hex")?;
    let ct_bytes = hex::decode(parts[1]).context("invalid ciphertext hex")?;
    let tag_bytes = hex::decode(parts[2]).context("invalid tag hex")?;

    if nonce_bytes.len() != NONCE_LEN {
        bail!(
            "invalid nonce length: expected {NONCE_LEN}, got {}",
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
        .decrypt(
            nonce,
            aes_gcm::aead::Payload {
                msg: combined.as_ref(),
                aad,
            },
        )
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
    let mut out = Zeroizing::new(vec![0u8; KEY_LEN]);
    hk.expand(domain, &mut out)
        .expect("32 bytes is valid HKDF-SHA256 output");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
    fn set_many_updates_all_values_in_one_transaction() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        let values = BTreeMap::from([
            ("A".to_string(), "one".to_string()),
            ("B".to_string(), "two".to_string()),
        ]);
        let payload = store.set_many("dev", &values).unwrap();
        assert_eq!(payload.version, 2);
        assert_eq!(payload.env_version("dev"), 2);
        assert_eq!(store.get("A", "dev").unwrap().as_deref(), Some("one"));
        assert_eq!(store.get("B", "dev").unwrap().as_deref(), Some("two"));
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

    fn payload_strategy() -> impl Strategy<Value = StorePayload> {
        (
            prop::collection::btree_map(any::<u8>(), any::<u32>(), 0..8),
            prop::collection::btree_map(any::<u8>(), any::<u64>(), 0..4),
            any::<u64>(),
        )
            .prop_map(|(secrets, tombstones, version)| {
                let secrets = secrets
                    .into_iter()
                    .map(|(key, value)| (format!("KEY{key}:dev"), value.to_string()))
                    .collect();
                let tombstones = tombstones
                    .into_iter()
                    .map(|(key, value)| (format!("DELETED{key}:dev"), value))
                    .collect();
                let mut env_versions = BTreeMap::new();
                env_versions.insert("dev".to_string(), version);
                StorePayload {
                    secrets,
                    version,
                    tombstones,
                    env_versions,
                    env_last_changed_at: BTreeMap::from([("dev".to_string(), version.to_string())]),
                }
            })
    }

    proptest! {
        #[test]
        fn encrypted_payload_roundtrips(payload in payload_strategy()) {
            let key = [0x42; KEY_LEN];
            let json = serde_json::to_string(&payload).unwrap();
            let encoded = encrypt_store_with_key(&key, &json).unwrap();
            let body = encoded.strip_prefix("v2:").unwrap();
            let decoded = decrypt_with_aad(&key, body, STORE_AAD_V2).unwrap();
            let restored: StorePayload = serde_json::from_str(&decoded).unwrap();

            prop_assert_eq!(restored.secrets, payload.secrets);
            prop_assert_eq!(restored.version, payload.version);
            prop_assert_eq!(restored.tombstones, payload.tombstones);
            prop_assert_eq!(restored.env_versions, payload.env_versions);
            prop_assert_eq!(restored.env_last_changed_at, payload.env_last_changed_at);
        }

        #[test]
        fn any_single_byte_ciphertext_component_flip_fails(
            payload in payload_strategy(),
            component in 0usize..3,
            byte_index in 0usize..64,
        ) {
            let key = [0x42; KEY_LEN];
            let json = serde_json::to_string(&payload).unwrap();
            let encoded = encrypt_store_with_key(&key, &json).unwrap();
            let body = encoded.strip_prefix("v2:").unwrap();
            let mut fields: Vec<Vec<u8>> = body
                .split(':')
                .map(|field| hex::decode(field).unwrap())
                .collect();
            let index = byte_index % fields[component].len();
            fields[component][index] ^= 0x01;
            let tampered = fields
                .iter()
                .map(hex::encode)
                .collect::<Vec<_>>()
                .join(":");

            prop_assert!(decrypt_with_aad(&key, &tampered, STORE_AAD_V2).is_err());
        }
    }

    #[test]
    fn store_files_use_versioned_aad_format() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        let encoded = std::fs::read_to_string(dir.path().join(".esk/store.enc")).unwrap();
        assert!(encoded.starts_with("v2:"));

        // A v2 ciphertext must not be accepted as the legacy unauthenticated-format API.
        let legacy_shape = encoded.strip_prefix("v2:").unwrap();
        let store = SecretStore::open(dir.path()).unwrap();
        assert!(store.decrypt(legacy_shape).is_err());
    }

    #[test]
    fn detects_rollback_against_local_high_water_mark() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "v1").unwrap();
        let old_store = std::fs::read(dir.path().join(".esk/store.enc")).unwrap();
        store.set("KEY", "dev", "v2").unwrap();
        std::fs::write(dir.path().join(".esk/store.enc"), old_store).unwrap();

        let err = store.payload().unwrap_err();
        assert!(err.to_string().contains("store rollback detected"));
    }

    #[test]
    fn rotates_key_without_changing_payload() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "sentinel").unwrap();
        let old_key = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();

        store.rotate_key().unwrap();

        let new_key = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();
        assert_ne!(old_key, new_key);
        let reopened = SecretStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened.get("KEY", "dev").unwrap().as_deref(),
            Some("sentinel")
        );
        assert!(decrypt_with_key(
            &hex::decode(old_key.trim()).unwrap(),
            &std::fs::read_to_string(dir.path().join(".esk/store.enc")).unwrap()
        )
        .is_err());
    }

    fn staged_rotation(dir: &tempfile::TempDir) -> (RotationStage, Zeroizing<Vec<u8>>) {
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "sentinel").unwrap();
        let payload = store.payload().unwrap();
        let esk_dir = dir.path().join(".esk");
        let provider = KeyProvider::from_marker(&esk_dir).unwrap();
        let stage =
            rotation_stage(&esk_dir, &provider, "0123456789abcdef0123456789abcdef").unwrap();
        let key = KeyProvider::generate_key();
        write_staged_key(&stage.staged_key, &key).unwrap();
        let encrypted =
            encrypt_store_with_key(&key, &serde_json::to_string(&payload).unwrap()).unwrap();
        write_private_new_file(&stage.candidate_store, encrypted.as_bytes()).unwrap();
        authenticate_store_file(&stage.candidate_store, &key).unwrap();
        write_rotation_journal(
            &esk_dir.join(ROTATION_JOURNAL_FILE),
            &RotationJournal {
                version: ROTATION_JOURNAL_VERSION,
                stage_id: "0123456789abcdef0123456789abcdef".to_string(),
                phase: RotationPhase::Prepared,
            },
        )
        .unwrap();
        (stage, key)
    }

    fn assert_rotation_recovered(dir: &tempfile::TempDir, stage: &RotationStage) {
        let reopened = SecretStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened.get("KEY", "dev").unwrap().as_deref(),
            Some("sentinel")
        );
        assert!(!stage.candidate_store.exists());
        if let KeyProvider::File { path } = &stage.staged_key {
            assert!(!path.exists());
        }
        assert!(!dir.path().join(".esk/key-rotation.json").exists());
    }

    #[test]
    fn rotation_journal_contains_only_version_stage_and_phase() {
        let journal = RotationJournal {
            version: ROTATION_JOURNAL_VERSION,
            stage_id: "0123456789abcdef0123456789abcdef".to_string(),
            phase: RotationPhase::Prepared,
        };
        let serialized = serde_json::to_string(&journal).unwrap();
        assert!(!serialized.contains("key"), "{serialized}");
        assert!(!serialized.contains("path"), "{serialized}");
    }

    #[test]
    fn recovers_rotation_crashed_before_store_promotion() {
        let dir = tmp_root();
        let (stage, _) = staged_rotation(&dir);
        assert_rotation_recovered(&dir, &stage);
    }

    #[test]
    fn recovers_rotation_crashed_after_store_promotion() {
        let dir = tmp_root();
        let (stage, _) = staged_rotation(&dir);
        let journal_path = dir.path().join(".esk/key-rotation.json");
        std::fs::rename(&stage.candidate_store, dir.path().join(".esk/store.enc")).unwrap();
        write_rotation_journal(
            &journal_path,
            &RotationJournal {
                version: ROTATION_JOURNAL_VERSION,
                stage_id: "0123456789abcdef0123456789abcdef".to_string(),
                phase: RotationPhase::StorePromoted,
            },
        )
        .unwrap();
        assert_rotation_recovered(&dir, &stage);
    }

    #[test]
    fn recovers_rotation_crashed_after_provider_promotion() {
        let dir = tmp_root();
        let (stage, key) = staged_rotation(&dir);
        let esk_dir = dir.path().join(".esk");
        std::fs::rename(&stage.candidate_store, esk_dir.join("store.enc")).unwrap();
        KeyProvider::from_marker(&esk_dir)
            .unwrap()
            .store(&key)
            .unwrap();
        write_rotation_journal(
            &esk_dir.join(ROTATION_JOURNAL_FILE),
            &RotationJournal {
                version: ROTATION_JOURNAL_VERSION,
                stage_id: "0123456789abcdef0123456789abcdef".to_string(),
                phase: RotationPhase::ProviderPromoted,
            },
        )
        .unwrap();
        assert_rotation_recovered(&dir, &stage);
    }

    #[test]
    fn recovers_rotation_with_environment_provider_using_durable_provider() {
        let dir = tmp_root();
        let (stage, key) = staged_rotation(&dir);
        let provider = KeyProvider::from_environment_value(&hex::encode(&key)).unwrap();
        let reopened = SecretStore::open_with_provider(dir.path(), &provider).unwrap();
        assert_eq!(
            reopened.get("KEY", "dev").unwrap().as_deref(),
            Some("sentinel")
        );
        assert!(!stage.candidate_store.exists());
        assert!(!dir.path().join(".esk/key-rotation.json").exists());
        let durable_key = KeyProvider::from_marker(&dir.path().join(".esk"))
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(&*durable_key, &*key);
    }

    #[test]
    fn forged_legacy_or_path_journals_fail_closed_without_touching_outside_files() {
        let dir = tmp_root();
        SecretStore::load_or_create(dir.path()).unwrap();
        let outside = dir.path().join("outside-sentinel");
        std::fs::write(&outside, "do not touch").unwrap();
        let journal_path = dir.path().join(".esk/key-rotation.json");

        std::fs::write(
            &journal_path,
            r#"{"new_key":"deadbeef","temp_store":"../outside-sentinel"}"#,
        )
        .unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid key rotation journal"));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not touch");

        std::fs::write(
            &journal_path,
            r#"{"version":1,"stage_id":"../../outside-sentinel","phase":"prepared"}"#,
        )
        .unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid key rotation stage identifier"));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not touch");

        std::fs::write(
            &journal_path,
            r#"{"version":1,"stage_id":"0123456789abcdef0123456789abcdef","phase":"unknown"}"#,
        )
        .unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid key rotation journal"));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not touch");
    }

    #[test]
    fn recovery_rejects_nonregular_staged_artifacts_before_mutating_live_store() {
        let dir = tmp_root();
        let (stage, _) = staged_rotation(&dir);
        std::fs::remove_file(&stage.candidate_store).unwrap();
        std::fs::create_dir(&stage.candidate_store).unwrap();
        let before = std::fs::read(dir.path().join(".esk/store.enc")).unwrap();

        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("unsafe staged rotation store"));
        assert_eq!(
            std::fs::read(dir.path().join(".esk/store.enc")).unwrap(),
            before
        );
    }

    #[test]
    fn recovery_rejects_directory_live_store_before_mutating_provider() {
        let dir = tmp_root();
        let (stage, _) = staged_rotation(&dir);
        let esk_dir = dir.path().join(".esk");
        let store_path = esk_dir.join("store.enc");
        let provider_path = esk_dir.join("store.key");
        let provider_before = std::fs::read(&provider_path).unwrap();
        let outside = dir.path().join("outside-sentinel");
        std::fs::write(&outside, "do not touch").unwrap();
        std::fs::remove_file(&stage.candidate_store).unwrap();
        std::fs::remove_file(&store_path).unwrap();
        std::fs::create_dir(&store_path).unwrap();

        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("unsafe promoted rotation store"));
        assert_eq!(std::fs::read(&provider_path).unwrap(), provider_before);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not touch");
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symlinked_live_store_before_mutating_provider_or_target() {
        use std::os::unix::fs::symlink;

        let dir = tmp_root();
        let (stage, _) = staged_rotation(&dir);
        let esk_dir = dir.path().join(".esk");
        let store_path = esk_dir.join("store.enc");
        let provider_path = esk_dir.join("store.key");
        let provider_before = std::fs::read(&provider_path).unwrap();
        let outside = dir.path().join("outside-sentinel");
        std::fs::write(&outside, "do not touch").unwrap();
        std::fs::remove_file(&stage.candidate_store).unwrap();
        std::fs::remove_file(&store_path).unwrap();
        symlink(&outside, &store_path).unwrap();

        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("unsafe promoted rotation store"));
        assert_eq!(std::fs::read(&provider_path).unwrap(), provider_before);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not touch");
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symlinked_staged_artifacts_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tmp_root();
        let (stage, _) = staged_rotation(&dir);
        let outside = dir.path().join("outside-sentinel");
        std::fs::write(&outside, "do not touch").unwrap();
        std::fs::remove_file(&stage.candidate_store).unwrap();
        symlink(&outside, &stage.candidate_store).unwrap();

        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("unsafe staged rotation store"));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not touch");
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
            ..Default::default()
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

    #[test]
    fn environment_provider_loads_store_without_key_file() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "value").unwrap();
        let key = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();
        std::fs::remove_file(dir.path().join(".esk/store.key")).unwrap();

        let provider = KeyProvider::from_environment_value(&format!("  {key}\n")).unwrap();
        let reopened = SecretStore::open_with_provider(dir.path(), &provider).unwrap();
        assert_eq!(
            reopened.get("KEY", "dev").unwrap(),
            Some("value".to_string())
        );
    }

    #[test]
    fn environment_provider_takes_precedence_over_file_key() {
        let dir = tmp_root();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("KEY", "dev", "value").unwrap();

        let wrong_key = hex::encode([0xa5_u8; KEY_LEN]);
        let provider = KeyProvider::from_environment_value(&wrong_key).unwrap();
        let reopened = SecretStore::open_with_provider(dir.path(), &provider).unwrap();
        let err = reopened.get("KEY", "dev").unwrap_err();
        assert!(err.to_string().contains("decryption failed"));
    }

    #[test]
    fn environment_provider_rejects_invalid_values() {
        for value in ["", "not-hex", "00"] {
            let err = KeyProvider::from_environment_value(value).err().unwrap();
            assert!(err.to_string().contains("invalid ESK_STORE_KEY"));
        }
    }

    #[test]
    fn environment_provider_cannot_rotate_or_store_a_new_key() {
        let provider = KeyProvider::from_environment_value(&hex::encode([0_u8; KEY_LEN])).unwrap();
        let err = provider.store(&[0_u8; KEY_LEN]).unwrap_err();
        assert!(err
            .to_string()
            .contains("cannot write a new encryption key"));
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
            version: 10,
            ..Default::default()
        };
        payload.env_versions.insert("dev".to_string(), 3);
        assert_eq!(payload.env_version("dev"), 3);
    }

    #[test]
    fn env_version_falls_back_to_global_when_no_env_versions() {
        let payload = StorePayload {
            version: 7,
            ..Default::default()
        };
        assert_eq!(payload.env_version("dev"), 7);
    }

    #[test]
    fn env_version_returns_zero_for_unknown_env() {
        let mut payload = StorePayload {
            version: 10,
            ..Default::default()
        };
        payload.env_versions.insert("dev".to_string(), 3);
        assert_eq!(payload.env_version("prod"), 0);
    }

    #[test]
    fn prune_tombstones_all_acknowledged() {
        use crate::sync_tracker::SyncIndex;

        let mut payload = StorePayload {
            version: 5,
            tombstones: BTreeMap::from([
                ("KEY_A:dev".to_string(), 2),
                ("KEY_B:dev".to_string(), 3),
            ]),
            ..Default::default()
        };
        let mut index = SyncIndex::new(Path::new("/tmp/test.json"));
        index.record_success("remote_a", "dev", 5);
        index.record_success("remote_b", "dev", 4);

        let pruned = payload.prune_tombstones(&index, &["remote_a", "remote_b"]);
        assert_eq!(pruned, 2);
        assert!(payload.tombstones.is_empty());
    }

    #[test]
    fn prune_tombstones_partially_acknowledged() {
        use crate::sync_tracker::SyncIndex;

        let mut payload = StorePayload {
            version: 5,
            tombstones: BTreeMap::from([
                ("KEY_A:dev".to_string(), 2),
                ("KEY_B:dev".to_string(), 4),
            ]),
            ..Default::default()
        };
        let mut index = SyncIndex::new(Path::new("/tmp/test.json"));
        index.record_success("remote_a", "dev", 3);

        let pruned = payload.prune_tombstones(&index, &["remote_a"]);
        assert_eq!(pruned, 1);
        assert_eq!(payload.tombstones.len(), 1);
        assert!(payload.tombstones.contains_key("KEY_B:dev"));
    }

    #[test]
    fn prune_tombstones_no_remotes() {
        let mut payload = StorePayload {
            version: 5,
            tombstones: BTreeMap::from([("KEY_A:dev".to_string(), 2)]),
            ..Default::default()
        };
        let index = crate::sync_tracker::SyncIndex::new(Path::new("/tmp/test.json"));

        let pruned = payload.prune_tombstones(&index, &[]);
        assert_eq!(pruned, 0);
        assert_eq!(payload.tombstones.len(), 1);
    }

    #[test]
    fn prune_tombstones_mixed_envs() {
        use crate::sync_tracker::SyncIndex;

        let mut payload = StorePayload {
            version: 5,
            tombstones: BTreeMap::from([
                ("KEY_A:dev".to_string(), 2),
                ("KEY_B:prod".to_string(), 3),
            ]),
            ..Default::default()
        };
        let mut index = SyncIndex::new(Path::new("/tmp/test.json"));
        index.record_success("remote_a", "dev", 5);
        // No record for prod

        let pruned = payload.prune_tombstones(&index, &["remote_a"]);
        assert_eq!(pruned, 1);
        assert!(!payload.tombstones.contains_key("KEY_A:dev"));
        assert!(payload.tombstones.contains_key("KEY_B:prod"));
    }

    #[test]
    fn prune_tombstones_empty_tombstones() {
        let mut payload = StorePayload {
            version: 5,
            ..Default::default()
        };
        let mut index = crate::sync_tracker::SyncIndex::new(Path::new("/tmp/test.json"));
        index.record_success("remote_a", "dev", 5);

        let pruned = payload.prune_tombstones(&index, &["remote_a"]);
        assert_eq!(pruned, 0);
    }
}
