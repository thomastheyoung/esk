# lockbox

Rust CLI for encrypted secrets management with multi-target sync.

## Architecture

```
src/
├── main.rs           # CLI entry point (clap)
├── lib.rs            # Library root (re-exports all modules)
├── config.rs         # YAML config parsing + validation
├── store.rs          # Encrypted secret store (AES-256-GCM)
├── tracker.rs        # Sync tracking (SHA-256 change detection)
├── reconcile.rs      # Version-based store reconciliation (pairwise + multi)
├── adapters/
│   ├── mod.rs        # SyncAdapter + CommandRunner traits, build_sync_adapters()
│   ├── env_file.rs   # .env file generation (batch sync)
│   ├── cloudflare.rs # wrangler secret put (individual sync)
│   └── convex.rs     # convex env set (individual sync)
├── plugins/
│   ├── mod.rs        # StoragePlugin trait, build_plugins()
│   ├── onepassword.rs # 1Password op CLI
│   └── cloud_file.rs # Cloud file storage (Dropbox, Google Drive, OneDrive)
├── cli/
│   ├── mod.rs        # Command routing
│   ├── init.rs       # lockbox init
│   ├── set.rs        # lockbox set
│   ├── get.rs        # lockbox get
│   ├── list.rs       # lockbox list
│   ├── sync.rs       # lockbox sync (adapter-agnostic)
│   ├── status.rs     # lockbox status (adapter-agnostic)
│   ├── push.rs       # lockbox push (plugin-agnostic)
│   └── pull.rs       # lockbox pull (plugin-agnostic + multi-reconciliation)
tests/
├── helpers/
│   └── mod.rs              # TestProject, fixtures, MockCommandRunner
├── store_integration.rs    # Store lifecycle tests (8)
├── reconcile_integration.rs # Reconcile flow tests (3)
├── env_file_integration.rs # Env file e2e tests (3)
└── cli_integration.rs      # CLI command tests (31)
```

## Core design

### Adapters vs plugins

Lockbox distinguishes between two extension types:

- **Adapters** deploy secrets to targets via `lockbox sync`. Secrets declare which adapters they target in `targets:`. Each adapter syncs individual secrets or batches.
- **Plugins** store/backup the entire secret list via `lockbox push`/`pull`. Plugins receive the full store payload per environment — no per-secret routing. Used for team sharing and backup.

### Config (`lockbox.yaml`)

Project-level config defines everything: environments, apps, adapter settings, plugin settings, and secrets. No hardcoded paths or project-specific assumptions in the binary.

### Encrypted store (`.secrets.enc`)

- AES-256-GCM (authenticated encryption — replaces CBC from the TS version)
- Random 32-byte key in `.secrets.key` (gitignored)
- Per-encryption 12-byte nonce
- Storage format: `nonce:ciphertext:tag` (hex-encoded)
- JSON payload: `{ "secrets": { "KEY:env": "value" }, "version": N }`
- Safe to commit to git

### Sync adapter trait

```rust
pub trait SyncAdapter {
    fn name(&self) -> &str;
    fn sync_mode(&self) -> SyncMode;  // Batch or Individual
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()>;
    fn sync_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<SyncResult>;
}
```

`SyncMode::Batch` adapters (env) regenerate the full output when any secret changes. `SyncMode::Individual` adapters (cloudflare, convex) sync one secret at a time. The `build_sync_adapters()` factory constructs all configured adapters from config.

### Storage plugin trait

```rust
pub trait StoragePlugin {
    fn name(&self) -> &str;
    fn push(&self, payload: &StorePayload, config: &Config, env: &str) -> Result<()>;
    fn pull(&self, config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>>;
}
```

Plugins receive the full store payload and operate per environment. The `build_plugins()` factory constructs all configured plugins from config.

### CommandRunner trait

Adapters and plugins that shell out to external CLIs (wrangler, convex, op) use the `CommandRunner` trait instead of `std::process::Command` directly. Production uses `RealCommandRunner`; tests inject `MockCommandRunner` to record calls and return canned responses.

```rust
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput>;
}
```

### Change tracking (`.sync-index.json`)

SHA-256 hash per (secret, target) pair. Skip sync when hash matches.
Atomic writes via temp file + rename.

### Reconciliation

Version-counter-based reconciliation between local store and remote plugins. Two modes:

- **Pairwise** (`reconcile()`): compares local store against a single remote source.
- **Multi-plugin** (`reconcile_multi()`): compares local store against N remote sources. Highest version wins as base; unique secrets from lower-version sources are merged in.

## Key crates

| Crate                               | Purpose                            |
| ----------------------------------- | ---------------------------------- |
| `clap`                              | CLI argument parsing with derive   |
| `serde`, `serde_yaml`, `serde_json` | Config and store serialization     |
| `aes-gcm`                           | Authenticated encryption           |
| `sha2`                              | Change detection hashing           |
| `dialoguer`                         | Interactive prompts (secret input) |
| `console`                           | Terminal colors and styling        |
| `tempfile`                          | Atomic file writes                 |
| `anyhow`                            | Error handling                     |

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
cargo test                    # Run all 223 tests
cargo test config::           # Run config unit tests only
cargo test store::            # Run store unit tests only
cargo test reconcile::        # Run reconcile unit tests only
cargo test tracker::          # Run tracker unit tests only
cargo test adapters::         # Run all adapter unit tests
cargo test plugins::          # Run all plugin unit tests
cargo test --test cli_integration  # Run CLI integration tests only
```

223 tests total: 161 unit (inline `#[cfg(test)]`) + 62 integration (`tests/`).

### Test infrastructure

- **`TestProject`** (`tests/helpers/mod.rs`): wraps `TempDir`, scaffolds valid lockbox project (writes `lockbox.yaml`, creates key/store files). Methods: `new(yaml)`, `with_store(yaml)`, `config()`, `store()`, `root()`, `sync_index_path()`.
- **Fixture constants**: `MINIMAL_CONFIG`, `FULL_CONFIG`, `ENV_ONLY_CONFIG`, `PLUGIN_CONFIG` — reusable YAML for tests.
- **`MockCommandRunner`**: records calls and returns configurable responses for adapter/plugin tests.
- Tests use `tempfile::TempDir` for isolation — no real external services.
- Never remove or weaken existing tests.
