# API reference

Complete command reference for lockbox.

## `lockbox init`

Initialize a new lockbox project in the current directory.

```bash
lockbox init
```

Creates:

- `lockbox.yaml` — scaffold config with example structure
- `.secrets.key` — random 32-byte encryption key (hex-encoded, `0600` permissions)
- `.secrets.enc` — empty encrypted store
- `.sync-index.json` — empty sync tracker

Idempotent — skips files that already exist. Warns if `.secrets.key` is not in `.gitignore`.

---

## `lockbox set`

Set a secret value for an environment.

```bash
lockbox set <KEY> --env <ENV> [--value <VALUE>] [--no-sync]
```

| Argument    | Required | Description                                                    |
| ----------- | -------- | -------------------------------------------------------------- |
| `KEY`       | Yes      | Secret key name (e.g., `STRIPE_SECRET_KEY`)                    |
| `--env`     | Yes      | Target environment                                             |
| `--value`   | No       | Secret value. If omitted, prompts interactively (hidden input) |
| `--no-sync` | No       | Skip the auto-sync that normally follows                       |

**Behavior:**

1. Validates the environment exists in config.
2. Warns if the key isn't defined in `lockbox.yaml` (but still allows it).
3. Stores the value in the encrypted store, incrementing the version counter.
4. If any plugins are configured, auto-pushes the environment's secrets to all plugins.
5. Runs `sync` for the affected environment (unless `--no-sync`).

**Examples:**

```bash
lockbox set API_KEY --env dev                     # Interactive prompt
lockbox set API_KEY --env dev --value sk_test_123 # Inline value
lockbox set API_KEY --env dev --no-sync           # Store only, don't sync
```

---

## `lockbox get`

Retrieve a secret value.

```bash
lockbox get <KEY> --env <ENV>
```

| Argument | Required | Description                  |
| -------- | -------- | ---------------------------- |
| `KEY`    | Yes      | Secret key name              |
| `--env`  | Yes      | Environment to retrieve from |

Prints the raw value to stdout. Exits with an error if the key/environment combination has no stored value.

**Examples:**

```bash
lockbox get STRIPE_SECRET_KEY --env dev
lockbox get DATABASE_URL --env prod | pbcopy  # Copy to clipboard
```

---

## `lockbox list`

List all secrets and their status.

```bash
lockbox list [--env <ENV>]
```

| Argument | Required | Description                    |
| -------- | -------- | ------------------------------ |
| `--env`  | No       | Filter to a single environment |

**Output:**

- Secrets grouped by vendor (as defined in `lockbox.yaml`).
- Each key shows which environments have stored values.
- Keys in the store but not in config appear under "Uncategorized".

**Example output:**

```
  Stripe
    STRIPE_SECRET_KEY  [dev, prod]
    STRIPE_WEBHOOK_SECRET  [dev]

  Convex
    CONVEX_DEPLOY_KEY  (no values)
```

---

## `lockbox sync`

Sync secrets to configured adapter targets.

```bash
lockbox sync [--env <ENV>] [--force] [--dry-run] [--verbose]
```

| Argument           | Required | Description                                        |
| ------------------ | -------- | -------------------------------------------------- |
| `--env`            | No       | Filter to a single environment                     |
| `--force`          | No       | Sync all secrets, ignoring change detection hashes |
| `--dry-run`        | No       | Show what would be synced without making changes   |
| `--verbose` / `-v` | No       | Show detailed output including skipped secrets     |

**Adapter behavior:**

- **Batch adapters** (env): Regenerate the entire output atomically when any secret in a target group changes.
- **Individual adapters** (cloudflare, convex): Sync one secret at a time via external CLI calls.

Targets whose adapter name matches a plugin (not an adapter) are skipped — plugins use `push`/`pull` instead.

**Change detection:**

SHA-256 hash of each secret value is tracked per (secret, adapter, app, environment) tuple in `.sync-index.json`. Secrets are skipped when the hash matches unless `--force` is used. Failed syncs are always retried.

**Example output:**

```
  synced STRIPE_SECRET_KEY:prod → cloudflare:web:prod
  skip   STRIPE_SECRET_KEY:dev → env:web:dev

  3 synced, 2 up to date
```

---

## `lockbox status`

Show sync status and drift for all configured adapter targets.

```bash
lockbox status [--env <ENV>]
```

| Argument | Required | Description                    |
| -------- | -------- | ------------------------------ |
| `--env`  | No       | Filter to a single environment |

Shows each (secret, target) pair with its sync state. Targets for plugins are excluded — only adapter targets are shown.

| Status         | Meaning                                             |
| -------------- | --------------------------------------------------- |
| `synced`       | Value hash matches last successful sync             |
| `pending`      | Value has changed since last sync                   |
| `never synced` | No sync record exists                               |
| `no value`     | Secret is defined in config but has no stored value |
| `failed`       | Last sync attempt failed (shows error)              |

Also displays the current store version number.

**Example output:**

```
  STRIPE_SECRET_KEY:dev → env:web:dev  synced
  STRIPE_SECRET_KEY:prod → cloudflare:web:prod  pending
  DATABASE_URL:dev → env:web:dev  never synced

  Store version: 5
```

---

## `lockbox push`

Push secrets to configured storage plugins.

```bash
lockbox push --env <ENV> [--only <PLUGIN>]
```

| Argument | Required | Description                    |
| -------- | -------- | ------------------------------ |
| `--env`  | Yes      | Environment to push            |
| `--only` | No       | Push to a specific plugin only |

**Requires:** At least one plugin configured in `lockbox.yaml` and its dependencies available (e.g., `op` CLI for 1Password).

Uploads all secrets for the given environment to each configured plugin. Without `--only`, pushes to all plugins. With `--only`, pushes to just the named plugin.

**Examples:**

```bash
lockbox push --env prod                     # Push to all plugins
lockbox push --env prod --only onepassword  # Push to 1Password only
lockbox push --env dev --only dropbox       # Push to Dropbox only
```

---

## `lockbox pull`

Pull secrets from configured storage plugins and reconcile with the local store.

```bash
lockbox pull --env <ENV> [--only <PLUGIN>] [--sync]
```

| Argument | Required | Description                      |
| -------- | -------- | -------------------------------- |
| `--env`  | Yes      | Environment to pull              |
| `--only` | No       | Pull from a specific plugin only |
| `--sync` | No       | Auto-run `sync` after pulling    |

**Requires:** At least one plugin configured in `lockbox.yaml` and its dependencies available.

Downloads secrets from all configured plugins (or just `--only <name>`) and reconciles with the local store using multi-plugin reconciliation:

1. Collects secrets and versions from all plugin sources.
2. The highest-version source becomes the base.
3. Unique secrets from lower-version sources are merged in.
4. Local store is updated with the merged result.
5. Plugins that were behind are updated with the merged result.

With `--sync`, automatically runs `lockbox sync --env <ENV>` after a successful pull.

**Examples:**

```bash
lockbox pull --env prod                   # Pull from all plugins + reconcile
lockbox pull --env prod --only onepassword  # Pull from 1Password only
lockbox pull --env prod --sync            # Pull + reconcile + sync targets
```

---

## Files

| File               | Description                             | Commit to git? |
| ------------------ | --------------------------------------- | -------------- |
| `lockbox.yaml`     | Project configuration                   | Yes            |
| `.secrets.enc`     | AES-256-GCM encrypted secret store      | Yes            |
| `.secrets.key`     | 32-byte encryption key (hex)            | **No**         |
| `.sync-index.json` | Sync state (hashes, timestamps, status) | Optional       |

## Exit codes

| Code | Meaning                                                         |
| ---- | --------------------------------------------------------------- |
| `0`  | Success                                                         |
| `1`  | Error (missing config, unknown environment, sync failure, etc.) |
