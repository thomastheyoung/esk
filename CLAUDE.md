# lockbox

Rust CLI for encrypted secrets management with multi-target sync.

## Architecture

```
src/
├── main.rs           # CLI entry point (clap)
├── config.rs         # YAML config parsing + validation
├── store.rs          # Encrypted secret store (AES-256-GCM)
├── tracker.rs        # Sync tracking (SHA-256 change detection)
├── reconcile.rs      # Version-based store reconciliation
├── adapters/
│   ├── mod.rs        # SyncAdapter trait
│   ├── env_file.rs   # .env file generation
│   ├── cloudflare.rs # wrangler secret put
│   ├── convex.rs     # convex env set
│   └── onepass.rs    # 1Password op CLI
└── cli/
    ├── mod.rs        # Command routing
    ├── init.rs       # lockbox init
    ├── set.rs        # lockbox set
    ├── get.rs        # lockbox get
    ├── list.rs       # lockbox list
    ├── sync.rs       # lockbox sync
    ├── status.rs     # lockbox status
    ├── push.rs       # lockbox push (1Password)
    └── pull.rs       # lockbox pull (1Password)
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
Adapters that shell out to external CLIs (wrangler, convex, op) use `std::process::Command`.

### Change tracking (`.sync-index.json`)

SHA-256 hash per (secret, target) pair. Skip sync when hash matches.
Atomic writes via temp file + rename.

### Reconciliation

Version-counter-based reconciliation between local store and 1Password.
Higher version wins. Same logic as the TS version.

## Key crates

| Crate | Purpose |
|-------|---------|
| `clap` | CLI argument parsing with derive |
| `serde`, `serde_yaml`, `serde_json` | Config and store serialization |
| `aes-gcm` | Authenticated encryption |
| `sha2` | Change detection hashing |
| `dialoguer` | Interactive prompts (secret input) |
| `console` | Terminal colors and styling |
| `tempfile` | Atomic file writes |
| `anyhow` | Error handling |

## Rules

- No hardcoded project names, paths, or assumptions — everything from config
- Adapters shell out to external CLIs (wrangler, convex, op) — don't reimplement their APIs
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
cargo test
```

Test against fixture configs, not real external services.
Mock adapter trait for sync tests.
