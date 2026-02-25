# esk

Rust CLI for encrypted secrets management with multi-target deploy.

## Architecture

```
src/
├── main.rs              # CLI entry point (clap)
├── lib.rs               # Library root (re-exports all modules)
├── config.rs            # YAML config parsing + validation
├── store.rs             # Encrypted secret store (AES-256-GCM)
├── deploy_tracker.rs    # Deploy tracking (SHA-256 change detection)
├── remote_tracker.rs    # Remote push tracking (version + status per remote/env)
├── reconcile.rs         # Version-based store reconciliation (pairwise + multi)
├── suggest.rs           # Typo suggestions (Levenshtein distance)
├── targets/
│   ├── mod.rs           # DeployTarget + CommandRunner traits, build_targets()
│   ├── env_file.rs      # .env file generation (batch deploy)
│   ├── cloudflare.rs    # wrangler secret put/delete (individual deploy)
│   ├── convex.rs        # convex env set/unset (individual deploy)
│   ├── fly.rs           # fly secrets import/unset (individual deploy, stdin)
│   ├── netlify.rs       # netlify env:set/unset (individual deploy)
│   ├── vercel.rs        # vercel env add/rm (individual deploy, stdin)
│   ├── github.rs        # gh secret set/delete (individual deploy, stdin)
│   ├── heroku.rs        # heroku config:set/unset (individual deploy)
│   ├── supabase.rs      # supabase secrets set/unset (individual deploy, stdin)
│   ├── railway.rs       # railway variables --set/delete (individual deploy)
│   ├── gitlab.rs        # glab variable set/delete (individual deploy, stdin)
│   ├── aws_ssm.rs       # aws ssm put-parameter/delete-parameter (individual deploy, stdin)
│   └── kubernetes.rs    # kubectl apply Secret manifest (batch deploy)
├── remotes/
│   ├── mod.rs           # SyncRemote trait, build_remotes()
│   ├── onepassword.rs   # 1Password op CLI
│   ├── cloud_file.rs    # Cloud file storage (Dropbox, Google Drive, OneDrive)
│   ├── aws_secrets_manager.rs  # AWS Secrets Manager
│   ├── hashicorp_vault.rs      # HashiCorp Vault KV
│   ├── bitwarden.rs     # Bitwarden Secrets Manager (bws CLI)
│   ├── s3.rs            # S3-compatible storage (AWS S3, R2, MinIO, DO Spaces)
│   ├── gcp_secret_manager.rs   # GCP Secret Manager
│   ├── azure_key_vault.rs      # Azure Key Vault
│   ├── doppler.rs       # Doppler secrets management
│   └── sops.rs          # Mozilla SOPS encrypted files
├── cli/
│   ├── mod.rs           # Command routing
│   ├── init.rs          # esk init
│   ├── set.rs           # esk set
│   ├── get.rs           # esk get
│   ├── delete.rs        # esk delete
│   ├── list.rs          # esk list
│   ├── deploy.rs        # esk deploy (target-agnostic)
│   ├── status.rs        # esk status (target-agnostic)
│   ├── generate.rs      # esk generate (TypeScript type declarations)
│   └── sync.rs          # esk sync (remote-agnostic, bidirectional)
tests/
├── helpers/
│   └── mod.rs              # TestProject, fixtures, MockCommandRunner
├── store_integration.rs    # Store lifecycle tests (8)
├── reconcile_integration.rs # Reconcile flow tests (3)
├── env_file_integration.rs # Env file e2e tests (3)
└── cli_integration.rs      # CLI command tests (122)
```

## Core design

### Targets vs remotes

esk distinguishes between two extension types:

- **Targets** deploy secrets to services via `esk deploy`. Secrets declare which targets they deploy to in `targets:`. Each target deploys individual secrets or batches.
- **Remotes** sync the entire secret list via `esk sync`. Remotes pull from remote, reconcile with local, and push merged results back — all in one bidirectional operation. Used for team sharing and backup.

### Config (`esk.yaml`)

Project-level config defines everything: environments, apps, target settings, remote settings, and secrets. No hardcoded paths or project-specific assumptions in the binary.

### Encrypted store (`.esk/store.enc`)

- AES-256-GCM (authenticated encryption)
- Random 32-byte key in `.esk/store.key` (gitignored)
- Per-encryption 12-byte nonce
- Storage format: `nonce:ciphertext:tag` (hex-encoded)
- JSON payload: `{ "secrets": { "KEY:env": "value" }, "version": N, "tombstones": { "KEY:env": N }, "env_versions": { "env": N } }`
- Safe to commit to git

### Deploy target trait

```rust
pub trait DeployTarget {
    fn name(&self) -> &str;
    fn deploy_mode(&self) -> DeployMode;  // Batch or Individual
    fn preflight(&self) -> Result<()>;  // Validate external deps (default: Ok)
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()>;
    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()>;  // Default: no-op
    fn sync_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<DeployResult>;
}
```

Batch targets handle deletion by regenerating the full output without the deleted key. Individual targets override `delete_secret` to call the external CLI's delete/unset command.

`DeployMode::Batch` targets (env, kubernetes) regenerate the full output when any secret changes. `DeployMode::Individual` targets deploy one secret at a time. The `build_targets()` factory constructs all configured targets from config, running preflight checks and filtering out targets that fail.

### Sync remote trait

```rust
pub trait SyncRemote {
    fn name(&self) -> &str;
    fn preflight(&self) -> Result<()>;  // Validate external deps (default: Ok)
    fn push(&self, payload: &StorePayload, config: &Config, env: &str) -> Result<()>;
    fn pull(&self, config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>>;
}
```

Remotes receive the full store payload and operate per environment. The `build_remotes()` factory constructs all configured remotes from config, running preflight checks and filtering out remotes that fail.

### CommandRunner trait

Targets and remotes that shell out to external CLIs (wrangler, convex, op) use the `CommandRunner` trait instead of `std::process::Command` directly. Production uses `RealCommandRunner`; tests inject `MockCommandRunner` to record calls and return canned responses.

```rust
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput>;
}
```

### Change tracking (`.esk/deploy-index.json`)

SHA-256 hash per (secret, target, app, environment) tuple. Skip deploy when hash matches.
Records include target, value hash, timestamp, deploy status (success/failed), and optional error.
Atomic writes via temp file + rename.

### Remote push tracking (`.esk/remote-index.json`)

Tracks push state per (remote, environment) pair. Records pushed version, timestamp, push status (success/failed), and optional error. Used by `status` to show remote push drift. Atomic writes via temp file + rename.

### Reconciliation

Version-counter-based reconciliation between local store and remote sources. Two modes:

- **Pairwise** (`reconcile()`): compares local store against a single remote source.
- **Multi-remote** (`reconcile_multi()`): compares local store against N remote sources. Highest version wins as base; unique secrets from lower-version sources are merged in.

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
| `chrono`                             | Timestamps in deploy records          |
| `cliclack`                          | Terminal UI (spinners, logs, prompts) |
| `console`                           | Terminal colors and styling           |
| `fs2`                               | File locking (exclusive store locks)  |
| `tempfile`                          | Atomic file writes                    |
| `anyhow`                            | Error handling                        |
| `thiserror`                         | Typed errors at API boundaries        |
| `zeroize`                           | Zeroing secret key bytes on drop      |

## Rules

- No hardcoded project names, paths, or assumptions — everything from config
- Targets shell out to external CLIs (wrangler, convex) via `CommandRunner` — don't reimplement their APIs
- Remotes shell out to external CLIs (op) or use filesystem operations via `CommandRunner`
- Prefer `anyhow` for error propagation, `thiserror` for typed errors at API boundaries
- Atomic file writes for store and deploy index (write to temp, rename)
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
cargo test                    # Run all tests
cargo test config::           # Run config unit tests only
cargo test store::            # Run store unit tests only
cargo test reconcile::        # Run reconcile unit tests only
cargo test deploy_tracker::   # Run deploy tracker unit tests only
cargo test remote_tracker::   # Run remote tracker unit tests only
cargo test suggest::          # Run suggest unit tests only
cargo test targets::          # Run all target unit tests
cargo test remotes::          # Run all remote unit tests
cargo test --test cli_integration  # Run CLI integration tests only
```

### Test infrastructure

- **`TestProject`** (`tests/helpers/mod.rs`): wraps `TempDir`, scaffolds valid esk project (writes `esk.yaml`, creates key/store files). Methods: `new(yaml)`, `with_store(yaml)`, `config()`, `store()`, `root()`, `deploy_index_path()`, `remote_index_path()`.
- **Fixture constants**: `MINIMAL_CONFIG`, `FULL_CONFIG`, `ENV_ONLY_CONFIG`, `PLUGIN_CONFIG`, `CLOUDFLARE_CONFIG`, `CONVEX_CONFIG`, `ONEPASSWORD_PLUGIN_CONFIG`, `FLY_CONFIG`, `NETLIFY_CONFIG`, `VERCEL_CONFIG`, `GITHUB_CONFIG`, `HEROKU_CONFIG`, `SUPABASE_CONFIG`, `RAILWAY_CONFIG`, `AWS_SSM_CONFIG`, `KUBERNETES_CONFIG`, `GITLAB_CONFIG` — reusable YAML for tests.
- **`MockCommandRunner`**: records calls and returns configurable responses for target/remote tests.
- Tests use `tempfile::TempDir` for isolation — no real external services.
- Never remove or weaken existing tests.
