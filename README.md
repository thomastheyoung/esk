# lockbox

Encrypted secrets management with multi-target sync. Store secrets locally with AES-256-GCM encryption, then sync them to `.env` files, Cloudflare Workers, Convex, and 1Password from a single source of truth.

## Why lockbox

- **One config, many targets** — Define a secret once, sync it to every service that needs it.
- **Encrypted at rest** — Secrets are AES-256-GCM encrypted. The store file (`.secrets.enc`) is safe to commit; the key file (`.secrets.key`) stays local.
- **Change detection** — SHA-256 hashing skips secrets that haven't changed. No unnecessary writes or API calls.
- **1Password as team remote** — Push/pull secrets to 1Password for team sharing, with version-based reconciliation.

## Quick start

```bash
# Build from source
cargo build --release

# Initialize a new project
lockbox init

# Set a secret
lockbox set API_KEY --env dev

# Sync to configured targets
lockbox sync
```

`lockbox init` creates four files:

| File | Purpose | Git |
|------|---------|-----|
| `lockbox.yaml` | Project config (environments, apps, adapters, secrets) | Commit |
| `.secrets.enc` | Encrypted secret store | Commit |
| `.secrets.key` | 32-byte encryption key (hex) | **Gitignore** |
| `.sync-index.json` | Sync state tracker | Optional |

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
  onepassword:
    vault: Engineering
    item_pattern: "{project} - {Environment}"

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

| Adapter | What it does | External CLI |
|---------|-------------|--------------|
| `env` | Generates `.env` files from a configurable path pattern | None |
| `cloudflare` | Runs `wrangler secret put` per secret | `wrangler` |
| `convex` | Runs `npx convex env set` per secret | `npx` |
| `onepassword` | Push/pull entire environment snapshots to 1Password items | `op` |

### Secrets

Organized by vendor (Stripe, AWS, etc.) for readability. Each secret declares which adapters and targets it syncs to.

Target format: `app:environment` (e.g., `web:prod`) or just `environment` for adapters that don't need an app context.

## Commands

| Command | Description |
|---------|-------------|
| `lockbox init` | Initialize encrypted store and config |
| `lockbox set <KEY>` | Set a secret value |
| `lockbox get <KEY>` | Retrieve a secret value |
| `lockbox list` | List all secrets and their status |
| `lockbox sync` | Sync secrets to configured targets |
| `lockbox status` | Show sync status and drift |
| `lockbox push` | Push secrets to 1Password |
| `lockbox pull` | Pull secrets from 1Password |

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

# Sync to all configured targets
lockbox sync
lockbox sync --env prod
lockbox sync --force          # Ignore change detection
lockbox sync --dry-run        # Preview without writing

# Check sync status
lockbox status
lockbox status --env dev

# 1Password team sharing
lockbox push --env prod       # Upload to 1Password
lockbox pull --env prod       # Download + reconcile
lockbox pull --env prod --sync  # Download + reconcile + sync targets
```

## Security model

- **Encryption**: AES-256-GCM with a random 12-byte nonce per write. Authenticated encryption prevents tampering.
- **Key file**: Random 32-byte key, hex-encoded, written with `0600` permissions on Unix.
- **Storage format**: `nonce:ciphertext:tag` (all hex-encoded). Nonce is never reused.
- **Memory**: Secret key bytes are zeroized on drop.
- **Atomic writes**: Store and sync index use temp file + rename to prevent corruption.

The encrypted store is safe to commit to git. The key file must never be committed — add `.secrets.key` to `.gitignore`.

## 1Password workflow

Lockbox uses 1Password as a team remote. Secrets are stored as items in a vault, with fields organized by vendor and a `_lockbox_version` field for reconciliation.

```bash
lockbox push --env prod    # Upload local secrets to 1Password
lockbox pull --env prod    # Download and reconcile
```

Reconciliation rules:
- **Remote is newer**: Pull remote secrets into local store. Push any local-only keys back.
- **Local is newer**: Advise to push.
- **Same version**: No action.

The `set` command auto-pushes to 1Password when the adapter is configured.

## Build

```bash
cargo build --release
```

The binary has no runtime dependencies beyond the external CLIs used by adapters (`wrangler`, `npx`, `op`).

## License

MIT
