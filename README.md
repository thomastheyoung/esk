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

| Command                | Description                                        |
| ---------------------- | -------------------------------------------------- |
| `lockbox init`         | Initialize encrypted store and config              |
| `lockbox set <KEY>`    | Set a secret value                                 |
| `lockbox get <KEY>`    | Retrieve a secret value                            |
| `lockbox delete <KEY>` | Delete a secret value                              |
| `lockbox list`         | List all secrets and their status                  |
| `lockbox sync`         | Sync secrets to configured adapter targets         |
| `lockbox status`       | Show sync status and drift                         |
| `lockbox push`         | Push secrets to configured plugins                 |
| `lockbox pull`         | Pull secrets from configured plugins and reconcile |

See [API.md](API.md) for the full command reference with all flags and behaviors.

## Usage

```bash
# Set secrets (interactive prompt for value)
lockbox set STRIPE_SECRET_KEY --env dev
lockbox set STRIPE_SECRET_KEY --env prod --value sk_live_...

# Retrieve a secret
lockbox get STRIPE_SECRET_KEY --env dev

# Delete a secret
lockbox delete STRIPE_SECRET_KEY --env dev

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

The `set` and `delete` commands automatically push to all configured plugins and sync to adapter targets (unless `--no-sync` is used).

## Troubleshooting

### Setup

| Error                                             | Cause                                | Fix                                                |
| ------------------------------------------------- | ------------------------------------ | -------------------------------------------------- |
| `encryption key not found at .lockbox/store.key`  | Store not initialized                | Run `lockbox init`                                 |
| `encrypted store not found at .lockbox/store.enc` | Store not initialized                | Run `lockbox init`                                 |
| `lockbox.yaml not found (searched from … upward)` | Not inside a lockbox project         | `cd` into your project root, or run `lockbox init` |
| `at least one environment must be defined`        | Empty `environments` array in config | Add at least one environment to `lockbox.yaml`     |

### Adapter preflight

These errors appear when running `lockbox sync` or `lockbox status`. Preflight checks verify that external CLIs are installed and authenticated before syncing.

| Error                                                | Cause                                                                                 | Fix                                                                                                                          |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `wrangler is not installed or not in PATH`           | `wrangler` CLI not found                                                              | `npm install -g wrangler`                                                                                                    |
| `wrangler is not authenticated. Run: wrangler login` | `wrangler whoami` failed                                                              | Run `wrangler login`                                                                                                         |
| `npx is not installed or not in PATH`                | Node.js not found                                                                     | Install Node.js                                                                                                              |
| `convex deployment not accessible: …`                | `convex env list` failed — bad auth, missing deployment, or wrong `deployment_source` | Check `convex` auth and that `deployment_source` in `lockbox.yaml` points to a valid `.env.local` with `CONVEX_DEPLOYMENT=…` |
| `Skipping {adapter} adapter: …`                      | Preflight failed — adapter excluded from sync                                         | Fix the underlying issue (see error detail); remaining adapters still sync                                                   |
| `No adapters available after preflight checks`       | All adapters failed preflight                                                         | Fix the errors printed above this message                                                                                    |

### Plugin preflight

These errors appear when running `lockbox push`, `lockbox pull`, or during auto-push from `lockbox set`/`delete`.

| Error                                                | Cause                                                                    | Fix                                                                                              |
| ---------------------------------------------------- | ------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------ |
| `1Password CLI (op) is not installed or not in PATH` | `op` CLI not found                                                       | [Install 1Password CLI](https://1password.com/downloads/command-line/)                           |
| `1Password vault '{vault}' not accessible: …`        | `op vault get` failed — not signed in, vault doesn't exist, or no access | Run `op signin` or check the `vault` name in `lockbox.yaml`                                      |
| `{name} sync folder not found at {path}`             | Cloud sync folder doesn't exist                                          | Install the cloud sync app (Dropbox, Google Drive, etc.) and verify the `path` in `lockbox.yaml` |
| `{name} sync folder at {path} is not writable: …`    | Cloud sync folder exists but isn't writable                              | Check file permissions on the sync folder                                                        |
| `Skipping {plugin} plugin: …`                        | Preflight failed — plugin excluded                                       | Fix the underlying issue; remaining plugins still run                                            |
| `No plugins available after preflight checks`        | All plugins failed preflight                                             | Fix the errors printed above this message                                                        |

### Sync failures

| Error                                        | Cause                                                                                                              | Fix                                                                             |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| `wrangler secret put failed for {KEY}: …`    | Cloudflare API rejected the secret write                                                                           | Check wrangler auth and that the Worker exists; the stderr detail has specifics |
| `wrangler secret delete failed for {KEY}: …` | Cloudflare API rejected the secret deletion                                                                        | Same as above                                                                   |
| `convex env set failed for {KEY}: …`         | Convex deployment rejected the env var write                                                                       | Check convex auth, deployment name, and `--prod` flag mapping in `env_flags`    |
| `convex env unset failed for {KEY}: …`       | Convex deployment rejected the env var deletion                                                                    | Same as above                                                                   |
| `cloudflare adapter requires an app`         | Secret targets cloudflare without specifying an app (e.g., `cloudflare: [dev]` instead of `cloudflare: [web:dev]`) | Use `app:env` format in `targets`                                               |
| `{N} sync(s) failed`                         | One or more secrets failed to sync                                                                                 | Scroll up for per-secret errors; fix and re-run `lockbox sync`                  |

### Push and pull failures

| Error                                                 | Cause                                                               | Fix                                                            |
| ----------------------------------------------------- | ------------------------------------------------------------------- | -------------------------------------------------------------- |
| `no plugins configured in lockbox.yaml`               | No `plugins:` section in config                                     | Add a plugin to `lockbox.yaml` (see [PLUGINS.md](PLUGINS.md))  |
| `unknown plugin '{name}'`                             | `--only` references a plugin that doesn't exist or failed preflight | Check the plugin name matches what's in `lockbox.yaml`         |
| `op item create failed: …` / `op item edit failed: …` | 1Password rejected the item write                                   | Check vault permissions and that the `op` session is active    |
| `{N} plugin push(es) failed`                          | One or more plugins failed during push                              | Run `lockbox push --env {env}` to retry after fixing the issue |
| `{N} plugin(s) failed to receive merged data`         | Push-back after pull reconciliation failed                          | Run `lockbox push --env {env}` to retry                        |

### Store and encryption

| Error                                                 | Cause                                                            | Fix                                                                                       |
| ----------------------------------------------------- | ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `decryption failed — wrong key or corrupted store`    | Key file doesn't match the encrypted store                       | Restore the correct `.lockbox/store.key` for this store, or pull from a plugin to recover |
| `invalid store format: expected nonce:ciphertext:tag` | `.lockbox/store.enc` is corrupt or was edited manually           | Restore from git or pull from a plugin                                                    |
| `secret '{KEY}' has no value for environment '{env}'` | Trying to delete a secret that doesn't exist in this environment | Check `lockbox list --env {env}` for current state                                        |

### Config validation

| Error                                                                  | Cause                                                            | Fix                                                              |
| ---------------------------------------------------------------------- | ---------------------------------------------------------------- | ---------------------------------------------------------------- |
| `secret '{KEY}' is defined in multiple vendors`                        | Same key name appears under two vendor sections                  | Move the secret to a single vendor section                       |
| `adapter '{name}' is not configured`                                   | Secret references an adapter that's not in the `adapters:` block | Add the adapter to `lockbox.yaml` or remove the target reference |
| `unknown environment '{env}' in target '{target}'`                     | Target references an env not in `environments`                   | Add the environment or fix the target                            |
| `unknown app '{app}' in target '{target}'`                             | Target references an app not in `apps`                           | Add the app or fix the target                                    |
| `'onepassword' should be configured under 'plugins:', not 'adapters:'` | 1Password placed in the wrong config section                     | Move the `onepassword` block from `adapters:` to `plugins:`      |

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
