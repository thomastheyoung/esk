# esk

Rust CLI for encrypted secrets management with multi-target sync.

## Architecture

```
src/
├── main.rs              # CLI entry point (clap)
├── lib.rs               # Library root (re-exports all modules)
├── config.rs            # YAML config parsing + validation
├── store.rs             # Encrypted secret store (AES-256-GCM)
├── tracker.rs           # Sync tracking (SHA-256 change detection)
├── plugin_tracker.rs    # Plugin push tracking (version + status per plugin/env)
├── reconcile.rs         # Version-based store reconciliation (pairwise + multi)
├── adapters/
│   ├── mod.rs           # SyncAdapter + CommandRunner traits, build_sync_adapters()
│   ├── env_file.rs      # .env file generation (batch sync)
│   ├── cloudflare.rs    # wrangler secret put/delete (individual sync)
│   ├── convex.rs        # convex env set/unset (individual sync)
│   ├── fly.rs           # fly secrets import/unset (individual sync, stdin)
│   ├── netlify.rs       # netlify env:set/unset (individual sync)
│   ├── vercel.rs        # vercel env add/rm (individual sync, stdin)
│   ├── github.rs        # gh secret set/delete (individual sync, stdin)
│   ├── heroku.rs        # heroku config:set/unset (individual sync)
│   ├── supabase.rs      # supabase secrets set/unset (individual sync, stdin)
│   ├── railway.rs       # railway variables --set/delete (individual sync)
│   ├── gitlab.rs        # glab variable set/delete (individual sync, stdin)
│   ├── aws_ssm.rs       # aws ssm put-parameter/delete-parameter (individual sync, stdin)
│   └── kubernetes.rs    # kubectl apply Secret manifest (batch sync)
├── plugins/
│   ├── mod.rs           # StoragePlugin trait, build_plugins()
│   ├── onepassword.rs   # 1Password op CLI
│   ├── cloud_file.rs    # Cloud file storage (Dropbox, Google Drive, OneDrive)
│   ├── aws_secrets_manager.rs  # AWS Secrets Manager
│   ├── vault.rs         # HashiCorp Vault KV
│   ├── bitwarden.rs     # Bitwarden Secrets Manager (bws CLI)
│   ├── s3.rs            # S3-compatible storage (AWS S3, R2, MinIO, DO Spaces)
│   ├── gcp.rs           # GCP Secret Manager
│   ├── azure.rs         # Azure Key Vault
│   ├── doppler.rs       # Doppler secrets management
│   └── sops.rs          # Mozilla SOPS encrypted files
├── cli/
│   ├── mod.rs           # Command routing
│   ├── init.rs          # esk init
│   ├── set.rs           # esk set
│   ├── get.rs           # esk get
│   ├── delete.rs        # esk delete
│   ├── list.rs          # esk list
│   ├── sync.rs          # esk sync (adapter-agnostic)
│   ├── status.rs        # esk status (adapter-agnostic)
│   ├── push.rs          # esk push (plugin-agnostic)
│   └── pull.rs          # esk pull (plugin-agnostic + multi-reconciliation)
tests/
├── helpers/
│   └── mod.rs              # TestProject, fixtures, MockCommandRunner
├── store_integration.rs    # Store lifecycle tests (8)
├── reconcile_integration.rs # Reconcile flow tests (3)
├── env_file_integration.rs # Env file e2e tests (3)
└── cli_integration.rs      # CLI command tests (115)
```

## Core design

### Adapters vs plugins

esk distinguishes between two extension types:

- **Adapters** deploy secrets to targets via `esk sync`. Secrets declare which adapters they target in `targets:`. Each adapter syncs individual secrets or batches.
- **Plugins** store/backup the entire secret list via `esk push`/`pull`. Plugins receive the full store payload per environment — no per-secret routing. Used for team sharing and backup.

### Config (`esk.yaml`)

Project-level config defines everything: environments, apps, adapter settings, plugin settings, and secrets. No hardcoded paths or project-specific assumptions in the binary.

### Encrypted store (`.esk/store.enc`)

- AES-256-GCM (authenticated encryption)
- Random 32-byte key in `.esk/store.key` (gitignored)
- Per-encryption 12-byte nonce
- Storage format: `nonce:ciphertext:tag` (hex-encoded)
- JSON payload: `{ "secrets": { "KEY:env": "value" }, "version": N, "tombstones": { "KEY:env": N }, "env_versions": { "env": N } }`
- Safe to commit to git

### Sync adapter trait

```rust
pub trait SyncAdapter {
    fn name(&self) -> &str;
    fn sync_mode(&self) -> SyncMode;  // Batch or Individual
    fn preflight(&self) -> Result<()>;  // Validate external deps (default: Ok)
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()>;
    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()>;  // Default: no-op
    fn sync_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<SyncResult>;
}
```

Batch adapters handle deletion by regenerating the full output without the deleted key. Individual adapters override `delete_secret` to call the external CLI's delete/unset command.

`SyncMode::Batch` adapters (env, kubernetes) regenerate the full output when any secret changes. `SyncMode::Individual` adapters sync one secret at a time. The `build_sync_adapters()` factory constructs all configured adapters from config, running preflight checks and filtering out adapters that fail.

### Storage plugin trait

```rust
pub trait StoragePlugin {
    fn name(&self) -> &str;
    fn preflight(&self) -> Result<()>;  // Validate external deps (default: Ok)
    fn push(&self, payload: &StorePayload, config: &Config, env: &str) -> Result<()>;
    fn pull(&self, config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>>;
}
```

Plugins receive the full store payload and operate per environment. The `build_plugins()` factory constructs all configured plugins from config, running preflight checks and filtering out plugins that fail.

### CommandRunner trait

Adapters and plugins that shell out to external CLIs (wrangler, convex, op) use the `CommandRunner` trait instead of `std::process::Command` directly. Production uses `RealCommandRunner`; tests inject `MockCommandRunner` to record calls and return canned responses.

```rust
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput>;
}
```

### Change tracking (`.esk/sync-index.json`)

SHA-256 hash per (secret, adapter, app, environment) tuple. Skip sync when hash matches.
Records include target, value hash, timestamp, sync status (success/failed), and optional error.
Atomic writes via temp file + rename.

### Plugin push tracking (`.esk/plugin-index.json`)

Tracks push state per (plugin, environment) pair. Records pushed version, timestamp, push status (success/failed), and optional error. Used by `status` to show plugin push drift. Atomic writes via temp file + rename.

### Reconciliation

Version-counter-based reconciliation between local store and remote plugins. Two modes:

- **Pairwise** (`reconcile()`): compares local store against a single remote source.
- **Multi-plugin** (`reconcile_multi()`): compares local store against N remote sources. Highest version wins as base; unique secrets from lower-version sources are merged in.

## Key crates

| Crate                               | Purpose                               |
| ----------------------------------- | ------------------------------------- |
| `clap`                              | CLI argument parsing with derive      |
| `serde`, `serde_yaml`, `serde_json` | Config and store serialization        |
| `aes-gcm`                           | Authenticated encryption              |
| `sha2`                              | Change detection hashing              |
| `hex`                               | Hex encoding for keys, nonces, hashes |
| `base64`                            | Base64 encoding for K8s secrets       |
| `rand`                              | Random key and nonce generation       |
| `atty`                              | TTY detection for interactive prompts |
| `chrono`                            | Timestamps in sync records            |
| `cliclack`                          | Terminal UI (spinners, logs, prompts) |
| `console`                           | Terminal colors and styling           |
| `fs2`                               | File locking (exclusive store locks)  |
| `tempfile`                          | Atomic file writes                    |
| `anyhow`                            | Error handling                        |
| `thiserror`                         | Typed errors at API boundaries        |
| `zeroize`                           | Zeroing secret key bytes on drop      |

## Rules

- No hardcoded project names, paths, or assumptions — everything from config
- Adapters shell out to external CLIs (wrangler, convex) via `CommandRunner` — don't reimplement their APIs
- Plugins shell out to external CLIs (op) or use filesystem operations via `CommandRunner`
- Prefer `anyhow` for error propagation, `thiserror` for typed errors at API boundaries
- Atomic file writes for store and sync index (write to temp, rename)
- Secrets in memory should be zeroized when possible (`zeroize` crate)
- No `unwrap()` on fallible operations — propagate errors

## Environments

Not limited to dev/prod. Defined in config, arbitrary names (dev, staging, prod, preview, etc.).

## Build and run

```bash
cargo build --release
cargo run -- <command>
```

## Testing

```bash
cargo test                    # Run all 611 tests
cargo test config::           # Run config unit tests only
cargo test store::            # Run store unit tests only
cargo test reconcile::        # Run reconcile unit tests only
cargo test tracker::          # Run tracker unit tests only
cargo test plugin_tracker::   # Run plugin tracker unit tests only
cargo test adapters::         # Run all adapter unit tests
cargo test plugins::          # Run all plugin unit tests
cargo test --test cli_integration  # Run CLI integration tests only
```

611 tests total: 482 unit (inline `#[cfg(test)]`) + 129 integration (`tests/`).

### Test infrastructure

- **`TestProject`** (`tests/helpers/mod.rs`): wraps `TempDir`, scaffolds valid esk project (writes `esk.yaml`, creates key/store files). Methods: `new(yaml)`, `with_store(yaml)`, `config()`, `store()`, `root()`, `sync_index_path()`, `plugin_index_path()`.
- **Fixture constants**: `MINIMAL_CONFIG`, `FULL_CONFIG`, `ENV_ONLY_CONFIG`, `PLUGIN_CONFIG`, `CLOUDFLARE_CONFIG`, `CONVEX_CONFIG`, `ONEPASSWORD_PLUGIN_CONFIG`, `FLY_CONFIG`, `NETLIFY_CONFIG`, `VERCEL_CONFIG`, `GITHUB_CONFIG`, `HEROKU_CONFIG`, `SUPABASE_CONFIG`, `RAILWAY_CONFIG`, `AWS_SSM_CONFIG`, `KUBERNETES_CONFIG`, `GITLAB_CONFIG` — reusable YAML for tests.
- **`MockCommandRunner`**: records calls and returns configurable responses for adapter/plugin tests.
- Tests use `tempfile::TempDir` for isolation — no real external services.
- Never remove or weaken existing tests.
