# lockbox

Encrypted secrets management with multi-target sync. Store secrets locally with AES-256-GCM encryption, then sync them to `.env` files, Cloudflare Workers, and Convex from a single source of truth. Back up and share secrets across your team with 1Password or cloud file storage.

## Why lockbox

- **One config, many targets** — Define a secret once, sync it to every service that needs it.
- **Encrypted at rest** — Secrets are AES-256-GCM encrypted. The store file (`.lockbox/store.enc`) is safe to commit; the key file (`.lockbox/store.key`) stays local.
- **Change detection** — SHA-256 hashing skips secrets that haven't changed. No unnecessary writes or API calls.
- **Pluggable storage** — Push/pull secrets to 1Password, Dropbox, Google Drive, or OneDrive for team sharing, with version-based reconciliation.

## Installation

**Shell script** (Linux/macOS):

```bash
curl -fsSL https://raw.githubusercontent.com/thomastheyoung/lockbox/main/install.sh | bash
```

**Cargo**:

```bash
cargo install lockbox        # build from source
cargo binstall lockbox       # download prebuilt binary
```

**From source**:

```bash
git clone https://github.com/thomastheyoung/lockbox.git
cd lockbox
cargo build --release
```

## Quick start

```bash
# Initialize a new project
lockbox init

# Set a secret
lockbox set API_KEY --env dev

# Sync to configured targets
lockbox sync
```

`lockbox init` creates four files:

| File                       | Purpose                                                         | Git           |
| -------------------------- | --------------------------------------------------------------- | ------------- |
| `lockbox.yaml`             | Project config (environments, apps, adapters, plugins, secrets) | Commit        |
| `.lockbox/store.enc`       | Encrypted secret store                                          | Commit        |
| `.lockbox/store.key`       | 32-byte encryption key (hex)                                    | **Gitignore** |
| `.lockbox/sync-index.json` | Sync state tracker                                              | Optional      |

## Configuration

All behavior is driven by `lockbox.yaml`. No hardcoded paths or project assumptions.

```yaml
project: myapp

environments: [dev, staging, prod]

apps:
  web:
    path: apps/web
  api:
    path: apps/api

adapters:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
      staging: ".staging"
      prod: ".production"
  cloudflare:
    env_flags:
      prod: "--env production"
  convex:
    path: apps/api
    deployment_source: apps/api/.env.local

plugins:
  onepassword:
    vault: Engineering
    item_pattern: "{project} - {Environment}"
  dropbox:
    type: cloud_file
    path: ~/Dropbox/secrets/myproject
    format: encrypted

secrets:
  Stripe:
    STRIPE_SECRET_KEY:
      description: Stripe API secret key
      targets:
        env: [web:dev, web:prod]
        cloudflare: [web:prod]
    STRIPE_WEBHOOK_SECRET:
      targets:
        env: [web:dev, web:prod]
  Convex:
    CONVEX_DEPLOY_KEY:
      targets:
        convex: [dev, prod]
```

### Environments

Arbitrary names — not limited to dev/prod. Define as many as your project needs.

### Apps

Named paths relative to the project root. Used by adapters that need to know where to run commands or write files.

### Adapters

Adapters deploy secrets to targets via `lockbox sync`. Each secret declares which adapters it targets.

| Adapter      | What it does                                            | External CLI |
| ------------ | ------------------------------------------------------- | ------------ |
| `env`        | Generates `.env` files from a configurable path pattern | None         |
| `cloudflare` | Runs `wrangler secret put` per secret                   | `wrangler`   |
| `convex`     | Runs `npx convex env set` per secret                    | `npx`        |

See [ADAPTERS.md](ADAPTERS.md) for detailed configuration of each adapter.

### Plugins

Plugins store and back up the entire secret list via `lockbox push`/`pull`. They operate on the full store per environment — no per-secret routing.

| Plugin                                             | What it does                                               | External CLI |
| -------------------------------------------------- | ---------------------------------------------------------- | ------------ |
| `onepassword`                                      | Push/pull environment snapshots to 1Password items         | `op`         |
| Cloud file (`dropbox`, `gdrive`, `onedrive`, etc.) | Sync encrypted or cleartext store to a cloud-synced folder | None         |

See [PLUGINS.md](PLUGINS.md) for detailed configuration of each plugin.

### Secrets

Organized by vendor (Stripe, AWS, etc.) for readability. Each secret declares which adapters and targets it syncs to.

Target format: `app:environment` (e.g., `web:prod`) or just `environment` for adapters that don't need an app context.

## Commands

| Command             | Description                                        |
| ------------------- | -------------------------------------------------- |
| `lockbox init`      | Initialize encrypted store and config              |
| `lockbox set <KEY>` | Set a secret value                                 |
| `lockbox get <KEY>` | Retrieve a secret value                            |
| `lockbox list`      | List all secrets and their status                  |
| `lockbox sync`      | Sync secrets to configured adapter targets         |
| `lockbox status`    | Show sync status and drift                         |
| `lockbox push`      | Push secrets to configured plugins                 |
| `lockbox pull`      | Pull secrets from configured plugins and reconcile |

See [API.md](API.md) for the full command reference with all flags and behaviors.

## Usage

```bash
# Set secrets (interactive prompt for value)
lockbox set STRIPE_SECRET_KEY --env dev
lockbox set STRIPE_SECRET_KEY --env prod --value sk_live_...

# Retrieve a secret
lockbox get STRIPE_SECRET_KEY --env dev

# List all secrets and their environments
lockbox list
lockbox list --env prod

# Sync to all configured adapter targets
lockbox sync
lockbox sync --env prod
lockbox sync --force          # Ignore change detection
lockbox sync --dry-run        # Preview without writing

# Check sync status
lockbox status
lockbox status --env dev

# Push/pull to storage plugins
lockbox push --env prod                   # Push to all plugins
lockbox push --env prod --only onepassword  # Push to specific plugin
lockbox pull --env prod                   # Pull from all plugins + reconcile
lockbox pull --env prod --only dropbox    # Pull from specific plugin
lockbox pull --env prod --sync            # Pull + auto-sync targets
```

## Security model

- **Encryption**: AES-256-GCM with a random 12-byte nonce per write. Authenticated encryption prevents tampering.
- **Key file**: Random 32-byte key, hex-encoded, written with `0600` permissions on Unix.
- **Storage format**: `nonce:ciphertext:tag` (all hex-encoded). Nonce is never reused.
- **Memory**: Secret key bytes are zeroized on drop.
- **Atomic writes**: Store and sync index use temp file + rename to prevent corruption.

The encrypted store is safe to commit to git. The key file must never be committed — add `.lockbox/store.key` to `.gitignore`.

## Plugin workflow

Lockbox plugins act as team remotes. Secrets are pushed to and pulled from one or more storage backends, with version-based reconciliation to handle concurrent edits.

```bash
lockbox push --env prod    # Upload local secrets to all plugins
lockbox pull --env prod    # Download from all plugins and reconcile
```

### Multi-plugin reconciliation

When pulling from multiple plugins, lockbox reconciles across all sources:

1. The source with the highest version becomes the base.
2. Unique secrets from lower-version sources are merged in.
3. Sources that were behind are updated with the merged result.

This means you can use 1Password for team sharing and Dropbox as a backup simultaneously — pull reconciles them all.

### Auto-push

The `set` command automatically pushes to all configured plugins after storing a secret (unless `--no-sync` is used).

## Development

### Sandbox environment

`cargo xtask sandbox` builds a release binary and scaffolds a test project in `/private/tmp/lockbox-test` with mock CLI shims and sample secrets — useful for manual testing without real external services.

```bash
cargo xtask sandbox          # build + scaffold + seed
cargo xtask sandbox --clean  # tear down
```

After setup, follow the printed instructions to `cd` and update your `PATH`.

## License

MIT
