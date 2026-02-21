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
├── reconcile.rs      # Version-based store reconciliation
├── adapters/
│   ├── mod.rs        # SyncAdapter + CommandRunner traits
│   ├── env_file.rs   # .env file generation
│   ├── cloudflare.rs # wrangler secret put
│   ├── convex.rs     # convex env set
│   └── onepassword.rs # 1Password op CLI
├── cli/
│   ├── mod.rs        # Command routing
│   ├── init.rs       # lockbox init
│   ├── set.rs        # lockbox set
│   ├── get.rs        # lockbox get
│   ├── list.rs       # lockbox list
│   ├── sync.rs       # lockbox sync
│   ├── status.rs     # lockbox status
│   ├── push.rs       # lockbox push (1Password)
│   └── pull.rs       # lockbox pull (1Password)
tests/
├── helpers/
│   └── mod.rs              # TestProject, fixtures, MockCommandRunner
├── store_integration.rs    # Store lifecycle tests (8)
├── reconcile_integration.rs # Reconcile flow tests (3)
├── env_file_integration.rs # Env file e2e tests (3)
└── cli_integration.rs      # CLI command tests (28)
```

## Core design

### Config (`lockbox.yaml`)

Project-level config defines everything: environments, apps, adapter settings, and secrets.
No hardcoded paths or project-specific assumptions in the binary.

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
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()>;
}
```

Each adapter is configured from the `adapters` section of `lockbox.yaml`.

### CommandRunner trait

Adapters that shell out to external CLIs (wrangler, convex, op) use the `CommandRunner` trait instead of `std::process::Command` directly. Production uses `RealCommandRunner`; tests inject `MockCommandRunner` to record calls and return canned responses.

```rust
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput>;
}
```

### Change tracking (`.sync-index.json`)

SHA-256 hash per (secret, target) pair. Skip sync when hash matches.
Atomic writes via temp file + rename.

### Reconciliation

Version-counter-based reconciliation between local store and 1Password.
Higher version wins. Same logic as the TS version.

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
- Adapters shell out to external CLIs (wrangler, convex, op) via `CommandRunner` — don't reimplement their APIs
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
cargo test                    # Run all 180 tests
cargo test config::           # Run config unit tests only
cargo test store::            # Run store unit tests only
cargo test reconcile::        # Run reconcile unit tests only
cargo test tracker::          # Run tracker unit tests only
cargo test adapters::         # Run all adapter unit tests
cargo test --test cli_integration  # Run CLI integration tests only
```

180 tests total: 138 unit (inline `#[cfg(test)]`) + 42 integration (`tests/`).

### Test infrastructure

- **`TestProject`** (`tests/helpers/mod.rs`): wraps `TempDir`, scaffolds valid lockbox project (writes `lockbox.yaml`, creates key/store files). Methods: `new(yaml)`, `with_store(yaml)`, `config()`, `store()`, `root()`, `sync_index_path()`.
- **Fixture constants**: `MINIMAL_CONFIG`, `FULL_CONFIG`, `ENV_ONLY_CONFIG` — reusable YAML for tests.
- **`MockCommandRunner`**: records calls and returns configurable responses for adapter tests.
- Tests use `tempfile::TempDir` for isolation — no real external services.
- Never remove or weaken existing tests.
