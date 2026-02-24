# API reference

Complete command reference for esk.

## `esk init`

Initialize a new esk project in the current directory.

```bash
esk init
```

Creates:

- `esk.yaml` — scaffold config with example structure
- `.esk/store.key` — random 32-byte encryption key (hex-encoded, `0600` permissions)
- `.esk/store.enc` — empty encrypted store
- `.esk/sync-index.json` — empty deploy tracker
- `.esk/plugin-index.json` — empty plugin push tracker

Idempotent — skips files that already exist. Warns if `.gitignore` exists but does not contain `.esk/store.key`.

---

## `esk delete`

Delete a secret value from an environment.

```bash
esk delete <KEY> --env <ENV> [--no-sync] [--strict]
```

| Argument    | Required | Description                                                            |
| ----------- | -------- | ---------------------------------------------------------------------- |
| `KEY`       | Yes      | Secret key name (e.g., `STRIPE_SECRET_KEY`)                            |
| `--env`     | Yes      | Environment to delete from                                             |
| `--no-sync` | No       | Store only — skip auto-push to plugins and auto-deploy                 |
| `--strict`  | No       | Fail if any plugin push fails and skip adapter deploy                  |

**Behavior:**

1. Validates the environment exists in config.
2. Warns if the key isn't defined in `esk.yaml`.
3. Removes the value from the encrypted store and records a tombstone, incrementing the version counter.
4. Unless `--no-sync`: auto-pushes the environment's secrets to all configured plugins.
5. Unless `--no-sync`: runs `deploy` for the affected environment (batch adapters regenerate without the deleted key; individual adapters call their delete command).
6. With `--strict`: if any plugin push fails, exits with an error and skips adapter deploy entirely.

**Examples:**

```bash
esk delete API_KEY --env dev                     # Delete + auto-deploy
esk delete API_KEY --env dev --no-sync           # Delete only, don't deploy
esk delete API_KEY --env dev --strict            # Fail hard on plugin errors
```

---

## `esk deploy`

Deploy secrets to configured adapter targets.

```bash
esk deploy [--env <ENV>] [--force] [--dry-run] [--verbose]
```

| Argument           | Required | Description                                          |
| ------------------ | -------- | ---------------------------------------------------- |
| `--env`            | No       | Filter to a single environment                       |
| `--force`          | No       | Deploy all secrets, ignoring change detection hashes |
| `--dry-run`        | No       | Show what would be deployed without making changes   |
| `--verbose` / `-v` | No       | Show detailed output including skipped secrets       |

**Adapter behavior:**

- **Batch adapters** (env): Regenerate the entire output atomically when any secret in a target group changes.
- **Individual adapters** (cloudflare, convex): Deploy one secret at a time via external CLI calls.

Targets whose adapter name matches a plugin (not an adapter) are skipped — plugins use `sync` instead.

**Change detection:**

SHA-256 hash of each secret value is tracked per (secret, adapter, app, environment) tuple in `.esk/sync-index.json`. Secrets are skipped when the hash matches unless `--force` is used. Failed deploys are always retried.

**Example output:**

```
  ✓ 2 synced
    STRIPE_SECRET_KEY:prod  → cloudflare:web
    STRIPE_WEBHOOK_SECRET:dev  → env:web

  3 up to date  (use --verbose to show)
```

---

## `esk set`

Set a secret value for an environment.

```bash
esk set <KEY> --env <ENV> [--value <VALUE>] [--group <GROUP>] [--no-sync] [--strict]
```

| Argument    | Required | Description                                                                |
| ----------- | -------- | -------------------------------------------------------------------------- |
| `KEY`       | Yes      | Secret key name (e.g., `STRIPE_SECRET_KEY`)                                |
| `--env`     | Yes      | Target environment                                                         |
| `--value`   | No       | Secret value. If omitted, prompts interactively (hidden input)             |
| `--group`   | No       | Config group to register the secret under (skips interactive prompt)        |
| `--no-sync` | No       | Store only — skip auto-push to plugins and auto-deploy                     |
| `--strict`  | No       | Fail if any plugin push fails and skip adapter deploy                      |

**Behavior:**

1. Validates the environment exists in config.
2. If the key isn't in `esk.yaml`:
   - With `--group`: adds the secret to that group in `esk.yaml` non-interactively.
   - Interactive mode (TTY, no `--group`): prompts "Add it?" with a group selector (existing groups or new).
   - Non-interactive mode (piped stdin, no `--group`): warns but proceeds.
3. Stores the value in the encrypted store, incrementing the version counter.
4. Unless `--no-sync`: auto-pushes the environment's secrets to all configured plugins.
5. Unless `--no-sync`: runs `deploy` for the affected environment.
6. With `--strict`: if any plugin push fails, exits with an error and skips adapter deploy entirely.

**Examples:**

```bash
esk set API_KEY --env dev                        # Interactive prompt for value
esk set API_KEY --env dev --value sk_test_123    # Inline value
esk set API_KEY --env dev --group Stripe          # Register under Stripe group
esk set API_KEY --env dev --no-sync              # Store only, don't deploy
esk set API_KEY --env dev --strict               # Fail hard on plugin errors
```

---

## `esk get`

Retrieve a secret value.

```bash
esk get <KEY> --env <ENV>
```

| Argument | Required | Description                  |
| -------- | -------- | ---------------------------- |
| `KEY`    | Yes      | Secret key name              |
| `--env`  | Yes      | Environment to retrieve from |

Prints the raw value to stdout. Exits with an error if the key/environment combination has no stored value.

**Examples:**

```bash
esk get STRIPE_SECRET_KEY --env dev
esk get DATABASE_URL --env prod | pbcopy  # Copy to clipboard
```

---

## `esk list`

List all secrets and their status.

```bash
esk list [--env <ENV>]
```

| Argument | Required | Description                    |
| -------- | -------- | ------------------------------ |
| `--env`  | No       | Filter to a single environment |

**Output:**

- Secrets grouped by vendor (as defined in `esk.yaml`), displayed as tables.
- Column headers show each environment.
- Per-cell status indicators reflect deploy state across all targets for that key/environment:
  - `✓` (green) — synced: all targets up to date.
  - `●` (yellow) — pending: value changed since last deploy.
  - `✗` (red) — failed: last deploy attempt failed.
  - `○` (dim) — unset: key is targeted for this environment but has no stored value.
  - Blank — not targeted: key has no configured targets for this environment.
- Keys in the store but not in config appear under "Uncategorized (not in esk.yaml)".

**Example output:**

```
  Stripe
                       dev  prod
  STRIPE_SECRET_KEY     ✓    ●
  STRIPE_WEBHOOK_SECRET ✓

  Convex
                       dev  prod
  CONVEX_DEPLOY_KEY     ○    ○
```

---

## `esk status`

Show status as an actionable dashboard.

```bash
esk status [--env <ENV>] [--all]
```

| Argument | Required | Description                                    |
| -------- | -------- | ---------------------------------------------- |
| `--env`  | No       | Filter to a single environment                 |
| `--all`  | No       | Show all targets including synced ones          |

Displays a multi-section dashboard with the following sections:

- **Summary** — Project name, store version, and target counts with status breakdown.
- **Targets** — Adapter health from preflight checks (pass/fail per adapter).
- **Sync** — Secrets grouped by status: failed, pending, unset, and synced (synced hidden unless `--all`). Each entry shows relative timestamps (e.g., "3h ago") and error details for failures.
- **Coverage** — Gaps where a secret is set in some environments but not others, and orphaned secrets (in store but not in config).
- **Plugins** — Push state per (plugin, environment): current, stale (version behind), failed, or never pushed.
- **Next steps** — Actionable commands to fix issues (retry failed deploys, deploy pending changes, fill coverage gaps, sync stale plugins, remove orphans).

The dashboard closes with the current store version.

**Example output:**

```
  myapp · v5 · 6 targets (3 synced, 2 pending, 1 unset)

  Targets
    ✓ env            writable
    ✓ cloudflare     wrangler authenticated

  Sync
    ● 2 pending
       STRIPE_SECRET_KEY:prod  → cloudflare:web  last synced 3h ago
       API_KEY:dev  → env:web  never synced
    ○ 1 unset
       DATABASE_URL:dev  → env:web:dev
    ✓ 3 synced  (--all to show)

  Next steps
    esk deploy --env prod  deploy 1 pending change
    esk deploy --env dev   deploy 1 pending change
    esk set DATABASE_URL --env dev  fill coverage gap

  Store version: 5
```

---

## `esk sync`

Sync secrets with configured storage plugins. Pulls from all plugins, reconciles with the local store, and pushes the merged result back to stale plugins.

```bash
esk sync --env <ENV> [--only <PLUGIN>] [--dry-run] [--strict] [--force] [--deploy]
```

| Argument   | Required | Description                                                               |
| ---------- | -------- | ------------------------------------------------------------------------- |
| `--env`    | Yes      | Environment to sync                                                       |
| `--only`   | No       | Sync a specific plugin only                                               |
| `--dry-run`| No       | Show what would change without modifying anything                         |
| `--strict` | No       | Fail if any plugin is unreachable (no partial reconciliation)             |
| `--force`  | No       | Bypass version jump protection — skip interactive prompt (use with caution)|
| `--deploy` | No       | Auto-run `deploy` after syncing                                           |

**Requires:** At least one plugin configured in `esk.yaml` and its dependencies available (e.g., `op` CLI for 1Password).

**Behavior:**

1. Pulls secrets and versions from all configured plugins (or just `--only <name>`).
2. The highest-version source becomes the base.
3. Unique secrets from lower-version sources are merged in.
4. Local store is updated with the merged result.
5. Stale plugins are automatically pushed the merged result (no interactive prompt).
6. With `--deploy`, automatically runs `esk deploy --env <ENV>` after a successful sync.
7. With `--dry-run`, shows what would change without modifying the store or pushing to plugins.

**Examples:**

```bash
esk sync --env prod                     # Sync all plugins
esk sync --env prod --only onepassword  # Sync specific plugin
esk sync --env prod --deploy            # Sync + auto-deploy targets
esk sync --env prod --dry-run           # Preview changes
```

---

## Files

| File                         | Description                               | Commit to git? |
| ---------------------------- | ----------------------------------------- | -------------- |
| `esk.yaml`               | Project configuration                     | Yes            |
| `.esk/store.enc`         | AES-256-GCM encrypted secret store        | Yes            |
| `.esk/store.key`         | 32-byte encryption key (hex)              | **No**         |
| `.esk/sync-index.json`   | Deploy state (hashes, timestamps, status) | Optional       |
| `.esk/plugin-index.json` | Plugin push state (versions, timestamps)  | Optional       |

## Exit codes

| Code | Meaning                                                         |
| ---- | --------------------------------------------------------------- |
| `0`  | Success                                                         |
| `1`  | Error (missing config, unknown environment, deploy failure, etc.) |
