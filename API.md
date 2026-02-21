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
4. If the 1Password adapter is configured, auto-pushes the environment's secrets.
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

- **env**: Regenerates the entire `.env` file atomically when any secret in an (app, env) pair changes. Groups secrets by vendor in the output.
- **cloudflare**: Runs `wrangler secret put <KEY>` with the value on stdin, in the app's directory.
- **convex**: Runs `npx convex env set <KEY> <VALUE>` in the configured path. Reads `CONVEX_DEPLOYMENT` from `deployment_source` if configured.
- **onepassword**: Skipped by sync — use `push`/`pull` instead.

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

Show sync status and drift for all configured targets.

```bash
lockbox status [--env <ENV>]
```

| Argument | Required | Description                    |
| -------- | -------- | ------------------------------ |
| `--env`  | No       | Filter to a single environment |

Shows each (secret, target) pair with its sync state:

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

Push secrets to 1Password.

```bash
lockbox push --env <ENV>
```

| Argument | Required | Description         |
| -------- | -------- | ------------------- |
| `--env`  | Yes      | Environment to push |

**Requires:** `onepassword` adapter configured in `lockbox.yaml` and the `op` CLI authenticated.

Uploads all secrets for the given environment to a 1Password item. The item name is derived from `item_pattern` in config (e.g., `"myapp - Prod"`).

- Creates the item if it doesn't exist; updates if it does.
- Secrets are stored as concealed fields, grouped by vendor under sections.
- A `_lockbox_version` field tracks the store version for reconciliation.

---

## `lockbox pull`

Pull secrets from 1Password and reconcile with the local store.

```bash
lockbox pull --env <ENV> [--sync]
```

| Argument | Required | Description                   |
| -------- | -------- | ----------------------------- |
| `--env`  | Yes      | Environment to pull           |
| `--sync` | No       | Auto-run `sync` after pulling |

**Requires:** `onepassword` adapter configured in `lockbox.yaml` and the `op` CLI authenticated.

Downloads secrets from the 1Password item and reconciles with the local store:

| Scenario               | Action                                                                |
| ---------------------- | --------------------------------------------------------------------- |
| Remote version > local | Merge remote secrets into local store. Push any local-only keys back. |
| Local version > remote | Print a message advising to `push`.                                   |
| Versions equal         | No action (already in sync).                                          |

With `--sync`, automatically runs `lockbox sync --env <ENV>` after a successful pull to propagate changes to all targets.

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
