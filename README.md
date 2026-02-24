# esk

Encrypted secrets management with multi-target deploy. Store secrets locally with AES-256-GCM encryption, then deploy them to `.env` files, Cloudflare Workers, Convex, Fly.io, Netlify, Vercel, GitHub Actions, Heroku, Supabase, Railway, GitLab CI, AWS SSM, and Kubernetes from a single source of truth. Back up and share secrets across your team with 1Password, cloud file storage, AWS Secrets Manager, HashiCorp Vault, Bitwarden, S3, GCP Secret Manager, Azure Key Vault, Doppler, or SOPS.

## Why esk

- **One config, many targets** — Define a secret once, deploy it to every service that needs it.
- **Encrypted at rest** — Secrets are AES-256-GCM encrypted. The store file (`.esk/store.enc`) is safe to commit; the key file (`.esk/store.key`) stays local.
- **Change detection** — SHA-256 hashing skips secrets that haven't changed. No unnecessary writes or API calls.
- **Pluggable storage** — Sync secrets with 1Password, Dropbox, Google Drive, OneDrive, AWS Secrets Manager, HashiCorp Vault, Bitwarden, S3, GCP Secret Manager, Azure Key Vault, Doppler, or SOPS for team sharing and backup, with version-based reconciliation.

## Installation

**Shell script** (Linux/macOS):

```bash
curl -fsSL https://raw.githubusercontent.com/thomastheyoung/esk/main/install.sh | bash
```

**Cargo**:

```bash
cargo install esk        # build from source
cargo binstall esk       # download prebuilt binary
```

**From source**:

```bash
git clone https://github.com/thomastheyoung/esk.git
cd esk
cargo build --release
```

## Quick start

```bash
# Initialize a new project
esk init

# Set a secret
esk set API_KEY --env dev

# Deploy to configured targets
esk deploy
```

`esk init` creates five files:

| File                         | Purpose                                                         | Git           |
| ---------------------------- | --------------------------------------------------------------- | ------------- |
| `esk.yaml`               | Project config (environments, apps, adapters, plugins, secrets) | Commit        |
| `.esk/store.enc`         | Encrypted secret store                                          | Commit        |
| `.esk/store.key`         | 32-byte encryption key (hex)                                    | **Gitignore** |
| `.esk/sync-index.json`   | Deploy state tracker                                            | Optional      |
| `.esk/plugin-index.json` | Plugin push state tracker                                       | Optional      |

## Configuration

All behavior is driven by `esk.yaml`. No hardcoded paths or project assumptions.

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

Adapters deploy secrets to targets via `esk deploy`. Each secret declares which adapters it targets.

| Adapter      | What it does                                            | External CLI |
| ------------ | ------------------------------------------------------- | ------------ |
| `env`        | Generates `.env` files from a configurable path pattern | None         |
| `cloudflare` | Runs `wrangler secret put` per secret                   | `wrangler`   |
| `convex`     | Runs `npx convex env set` per secret                    | `npx`        |
| `fly`        | Runs `fly secrets import` per secret (stdin)            | `fly`        |
| `netlify`    | Runs `netlify env:set` per secret                       | `netlify`    |
| `vercel`     | Runs `vercel env add` per secret (stdin)                | `vercel`     |
| `github`     | Runs `gh secret set` per secret (stdin)                 | `gh`         |
| `heroku`     | Runs `heroku config:set` per secret                     | `heroku`     |
| `supabase`   | Runs `supabase secrets set` per secret                  | `supabase`   |
| `railway`    | Runs `railway variables --set` per secret               | `railway`    |
| `gitlab`     | Runs `glab variable set` per secret                     | `glab`       |
| `aws_ssm`   | Runs `aws ssm put-parameter` per secret (stdin)         | `aws`        |
| `kubernetes` | Generates and applies K8s Secret manifests              | `kubectl`    |

See [ADAPTERS.md](ADAPTERS.md) for detailed configuration of each adapter.

### Plugins

Plugins sync the entire secret list via `esk sync`. They pull from remote, reconcile with the local store, and push merged results back — all in one bidirectional operation. Used for team sharing and backup.

| Plugin                                             | What it does                                               | External CLI |
| -------------------------------------------------- | ---------------------------------------------------------- | ------------ |
| `onepassword`                                      | Push/pull environment snapshots to 1Password items         | `op`         |
| Cloud file (`dropbox`, `gdrive`, `onedrive`, etc.) | Sync encrypted or cleartext store to a cloud-synced folder | None         |
| `aws_secrets_manager`                              | Store secrets as JSON in AWS Secrets Manager               | `aws`        |
| `vault`                                            | Store secrets in HashiCorp Vault KV                        | `vault`      |
| `bitwarden`                                        | Store secrets in Bitwarden Secrets Manager                 | `bws`        |
| `s3`                                               | Store encrypted or cleartext files in S3-compatible storage| `aws`        |
| `gcp`                                              | Store secrets in GCP Secret Manager                        | `gcloud`     |
| `azure`                                            | Store secrets in Azure Key Vault                           | `az`         |
| `doppler`                                          | Sync secrets to Doppler projects                           | `doppler`    |
| `sops`                                             | Store as SOPS-encrypted files                              | `sops`       |

See [PLUGINS.md](PLUGINS.md) for detailed configuration of each plugin.

### Secrets

Organized by vendor (Stripe, AWS, etc.) for readability. Each secret declares which adapters and targets it deploys to.

Target format: `app:environment` (e.g., `web:prod`) or just `environment` for adapters that don't need an app context.

## Commands

| Command                | Description                                        |
| ---------------------- | -------------------------------------------------- |
| `esk init`         | Initialize encrypted store and config              |
| `esk set <KEY>`    | Set a secret value (with optional config registration) |
| `esk get <KEY>`    | Retrieve a secret value                            |
| `esk delete <KEY>` | Delete a secret value                              |
| `esk list`         | List all secrets with per-environment deploy status |
| `esk deploy`       | Deploy secrets to configured adapter targets       |
| `esk status`       | Show actionable deploy/sync dashboard              |
| `esk sync`         | Sync secrets with configured plugins (pull, reconcile, push) |

See [API.md](API.md) for the full command reference with all flags and behaviors.

## Usage

```bash
# Set secrets (interactive prompt for value)
esk set STRIPE_SECRET_KEY --env dev
esk set STRIPE_SECRET_KEY --env prod --value sk_live_...
esk set NEW_KEY --env dev --group Stripe  # Register in config under Stripe group

# Retrieve a secret
esk get STRIPE_SECRET_KEY --env dev

# Delete a secret
esk delete STRIPE_SECRET_KEY --env dev

# List all secrets and their environments
esk list
esk list --env prod

# Deploy to all configured adapter targets
esk deploy
esk deploy --env prod
esk deploy --force          # Ignore change detection
esk deploy --dry-run        # Preview without writing
esk deploy --verbose        # Show skipped secrets too

# Check status
esk status
esk status --env dev
esk status --all          # Include synced targets in output

# Sync with storage plugins (pull + reconcile + push)
esk sync --env prod                     # Sync all plugins
esk sync --env prod --only onepassword  # Sync specific plugin
esk sync --env prod --deploy            # Sync + auto-deploy targets
esk sync --env prod --strict            # Fail if any plugin is unreachable
esk sync --env prod --force             # Bypass version jump protection
esk sync --env prod --dry-run           # Preview without modifying anything
```

## Security model

- **Encryption**: AES-256-GCM with a random 12-byte nonce per write. Authenticated encryption prevents tampering.
- **Key file**: Random 32-byte key, hex-encoded. The `.esk/` directory is created with `0700` permissions; the key file and encrypted store are both written with `0600` permissions on Unix.
- **Storage format**: `nonce:ciphertext:tag` (all hex-encoded). Nonce is never reused.
- **Memory**: Secret key bytes are zeroized on drop.
- **Atomic writes**: Store and sync index use temp file + rename to prevent corruption.

The encrypted store is safe to commit to git. The key file must never be committed — add `.esk/store.key` to `.gitignore`.

## Plugin workflow

Esk plugins act as team remotes. Secrets are synced with one or more storage backends, with version-based reconciliation to handle concurrent edits.

```bash
esk sync --env prod    # Pull from all plugins, reconcile, push merged result back
```

### Multi-plugin reconciliation

When syncing with multiple plugins, esk reconciles across all sources:

1. The source with the highest version becomes the base.
2. Unique secrets from lower-version sources are merged in.
3. Sources that were behind are updated with the merged result.

This means you can use 1Password for team sharing and Dropbox as a backup simultaneously — sync reconciles them all.

### Auto-push

The `set` and `delete` commands automatically push to all configured plugins and deploy to adapter targets (unless `--no-sync` is used). Use `--strict` on either command to fail immediately if any plugin push fails, skipping adapter deploy entirely.

## Troubleshooting

### Setup

| Error                                             | Cause                                | Fix                                                |
| ------------------------------------------------- | ------------------------------------ | -------------------------------------------------- |
| `encryption key not found at .esk/store.key`  | Store not initialized                | Run `esk init`                                 |
| `encrypted store not found at .esk/store.enc` | Store not initialized                | Run `esk init`                                 |
| `esk.yaml not found (searched from … upward)` | Not inside an esk project         | `cd` into your project root, or run `esk init` |
| `at least one environment must be defined`        | Empty `environments` array in config | Add at least one environment to `esk.yaml`     |

### Adapter preflight

These errors appear when running `esk deploy` or `esk status`. Preflight checks verify that external CLIs are installed and authenticated before deploying.

| Error                                                | Cause                                                                                 | Fix                                                                                                                          |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `wrangler is not installed or not in PATH`           | `wrangler` CLI not found                                                              | `npm install -g wrangler`                                                                                                    |
| `wrangler is not authenticated. Run: wrangler login` | `wrangler whoami` failed                                                              | Run `wrangler login`                                                                                                         |
| `npx is not installed or not in PATH`                | Node.js not found                                                                     | Install Node.js                                                                                                              |
| `convex deployment not accessible: …`                | `convex env list` failed — bad auth, missing deployment, or wrong `deployment_source` | Check `convex` auth and that `deployment_source` in `esk.yaml` points to a valid `.env.local` with `CONVEX_DEPLOYMENT=…` |
| `Skipping {adapter} adapter: …`                      | Preflight failed — adapter excluded from deploy                                       | Fix the underlying issue (see error detail); remaining adapters still deploy                                                 |
| `No adapters available after preflight checks`       | All adapters failed preflight                                                         | Fix the errors printed above this message                                                                                    |

### Plugin preflight

These errors appear when running `esk sync`, or during auto-push from `esk set`/`delete`.

| Error                                                | Cause                                                                    | Fix                                                                                              |
| ---------------------------------------------------- | ------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------ |
| `1Password CLI (op) is not installed or not in PATH` | `op` CLI not found                                                       | [Install 1Password CLI](https://1password.com/downloads/command-line/)                           |
| `1Password vault '{vault}' not accessible: …`        | `op vault get` failed — not signed in, vault doesn't exist, or no access | Run `op signin` or check the `vault` name in `esk.yaml`                                      |
| `{name} sync folder not found at {path}`             | Cloud sync folder doesn't exist                                          | Install the cloud sync app (Dropbox, Google Drive, etc.) and verify the `path` in `esk.yaml` |
| `{name} sync folder at {path} is not writable: …`    | Cloud sync folder exists but isn't writable                              | Check file permissions on the sync folder                                                        |
| `Skipping {plugin} plugin: …`                        | Preflight failed — plugin excluded                                       | Fix the underlying issue; remaining plugins still run                                            |
| `No plugins available after preflight checks`        | All plugins failed preflight                                             | Fix the errors printed above this message                                                        |

### Deploy failures

| Error                                        | Cause                                                                                                              | Fix                                                                             |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| `wrangler secret put failed for {KEY}: …`    | Cloudflare API rejected the secret write                                                                           | Check wrangler auth and that the Worker exists; the stderr detail has specifics |
| `wrangler secret delete failed for {KEY}: …` | Cloudflare API rejected the secret deletion                                                                        | Same as above                                                                   |
| `convex env set failed for {KEY}: …`         | Convex deployment rejected the env var write                                                                       | Check convex auth, deployment name, and `--prod` flag mapping in `env_flags`    |
| `convex env unset failed for {KEY}: …`       | Convex deployment rejected the env var deletion                                                                    | Same as above                                                                   |
| `cloudflare adapter requires an app`         | Secret targets cloudflare without specifying an app (e.g., `cloudflare: [dev]` instead of `cloudflare: [web:dev]`) | Use `app:env` format in `targets`                                               |
| `{N} sync(s) failed`                         | One or more secrets failed to deploy                                                                               | Scroll up for per-secret errors; fix and re-run `esk deploy`                |

### Sync failures

| Error                                                 | Cause                                                               | Fix                                                            |
| ----------------------------------------------------- | ------------------------------------------------------------------- | -------------------------------------------------------------- |
| `no plugins configured in esk.yaml`               | No `plugins:` section in config                                     | Add a plugin to `esk.yaml` (see [PLUGINS.md](PLUGINS.md))  |
| `unknown plugin '{name}'`                             | `--only` references a plugin that doesn't exist or failed preflight | Check the plugin name matches what's in `esk.yaml`         |
| `op item create failed: …` / `op item edit failed: …` | 1Password rejected the item write                                   | Check vault permissions and that the `op` session is active    |
| `{N} plugin push(es) failed`                          | One or more plugins failed during push                              | Run `esk sync --env {env}` to retry after fixing the issue |
| `{N} plugin(s) failed to receive merged data`         | Push-back after reconciliation failed                               | Run `esk sync --env {env}` to retry                        |

### Store and encryption

| Error                                                 | Cause                                                            | Fix                                                                                       |
| ----------------------------------------------------- | ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `decryption failed — wrong key or corrupted store`    | Key file doesn't match the encrypted store                       | Restore the correct `.esk/store.key` for this store, or sync from a plugin to recover |
| `invalid store format: expected nonce:ciphertext:tag` | `.esk/store.enc` is corrupt or was edited manually           | Restore from git or sync from a plugin                                                    |
| `secret '{KEY}' has no value for environment '{env}'` | Trying to delete a secret that doesn't exist in this environment | Check `esk list --env {env}` for current state                                        |

### Config validation

| Error                                                                  | Cause                                                            | Fix                                                              |
| ---------------------------------------------------------------------- | ---------------------------------------------------------------- | ---------------------------------------------------------------- |
| `secret '{KEY}' is defined in multiple vendors`                        | Same key name appears under two vendor sections                  | Move the secret to a single vendor section                       |
| `adapter '{name}' is not configured`                                   | Secret references an adapter that's not in the `adapters:` block | Add the adapter to `esk.yaml` or remove the target reference |
| `unknown environment '{env}' in target '{target}'`                     | Target references an env not in `environments`                   | Add the environment or fix the target                            |
| `unknown app '{app}' in target '{target}'`                             | Target references an app not in `apps`                           | Add the app or fix the target                                    |
| `'onepassword' should be configured under 'plugins:', not 'adapters:'` | 1Password placed in the wrong config section                     | Move the `onepassword` block from `adapters:` to `plugins:`      |

## Development

### Sandbox environment

`cargo xtask sandbox` builds a release binary and scaffolds a test project in `/private/tmp/esk-test` with mock CLI shims and sample secrets — useful for manual testing without real external services.

```bash
cargo xtask sandbox          # build + scaffold + seed
cargo xtask sandbox --clean  # tear down
```

After setup, follow the printed instructions to `cd` and update your `PATH`.

## License

MIT
