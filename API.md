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
- `.esk/deploy-index.json` — empty deploy tracker
- `.esk/sync-index.json` — empty sync tracker

Idempotent — skips files that already exist. Updates `.gitignore` to include:

```gitignore
# esk (store.enc is safe to commit)
.esk/store.key
.esk/deploy-index.json
.esk/sync-index.json
```

---

## `esk delete`

Delete a secret value from an environment.

```bash
esk delete <KEY> --env <ENV> [--no-sync] [--bail]
```

| Argument    | Required | Description                                            |
| ----------- | -------- | ------------------------------------------------------ |
| `KEY`       | Yes      | Secret key name (e.g., `STRIPE_SECRET_KEY`)            |
| `--env`     | Yes      | Environment to delete from                             |
| `--no-sync` | No       | Store only — skip auto-push to remotes and auto-deploy |
| `--bail`    | No       | Fail if any remote push fails and skip deploy          |

**Behavior:**

1. Validates the environment exists in config.
2. Warns if the key isn't defined in `esk.yaml`.
3. Removes the value from the encrypted store and records a tombstone, incrementing the version counter. Errors if the key has no stored value for the environment.
4. Unless `--no-sync`: auto-pushes the environment's secrets to all configured remotes.
5. Unless `--no-sync`: runs `deploy` for the affected environment (batch targets regenerate without the deleted key; individual targets call their delete command).
6. With `--bail`: if any remote push fails, exits with an error and skips deploy entirely.
7. Without `--bail`: deploy still runs, but the command exits non-zero if any remote push failed (to surface retry work).

**Examples:**

```bash
esk delete API_KEY --env dev                     # Delete + auto-deploy
esk delete API_KEY --env dev --no-sync           # Store only, skip sync and deploy
esk delete API_KEY --env dev --bail              # Fail hard on remote errors
```

---

## `esk deploy`

Deploy secrets to configured targets.

```bash
esk deploy [--env <ENV>] [--force] [--dry-run] [--verbose] [--skip-validation] [--skip-requirements] [--allow-empty]
```

| Argument              | Required | Description                                          |
| --------------------- | -------- | ---------------------------------------------------- |
| `--env`               | No       | Filter to a single environment                       |
| `--force`             | No       | Deploy all secrets, ignoring change detection hashes |
| `--dry-run`           | No       | Show what would be deployed without making changes   |
| `--verbose` / `-v`    | No       | Show detailed output including skipped secrets       |
| `--skip-validation`   | No       | Bypass `validate:` checks before deploying           |
| `--skip-requirements` | No       | Bypass `required:` checks (missing secret errors)    |
| `--allow-empty`       | No       | Allow deploying empty/whitespace-only values         |

**Pre-deploy checks:**

Before deploying, esk runs three checks on the secrets in scope (unless bypassed):

1. **Validation** — secrets with a `validate:` block are checked against their constraints. Failures abort deploy. Bypass with `--skip-validation`.
2. **Requirements** — secrets with `required: true` (or a matching env list) must have a stored value. Missing secrets abort deploy. Bypass with `--skip-requirements` or `--force`.
3. **Empty values** — secrets with empty or whitespace-only values are flagged. In non-interactive/CI contexts, empty values abort deploy. Bypass with `--allow-empty`. Secrets with `allow_empty: true` in config are always exempt.

**Target behavior:**

- **Batch targets** (`env`, `kubernetes`): Regenerate the entire output atomically when any secret in a target group changes.
- **Individual targets** (for example `cloudflare`, `convex`): Deploy one secret at a time via external CLI calls.

Targets that fail preflight checks are skipped with warnings (or all deploy work is skipped if no targets remain available).

**Change detection:**

SHA-256 hash of each secret value is tracked per (secret, target, app, environment) tuple in `.esk/deploy-index.json`. Secrets are skipped when the hash matches unless `--force` is used. Failed deploys are always retried.

**Example output:**

```
  ✔ 2 deployed
    STRIPE_SECRET_KEY:prod  → cloudflare:web
    STRIPE_WEBHOOK_SECRET:dev  → env:web

  3 targets up to date  (use --verbose to show)
```

---

## `esk set`

Set a secret value for an environment.

```bash
esk set <KEY> --env <ENV> [--value <VALUE>] [--group <GROUP>] [--no-sync] [--bail] [--skip-validation] [--force]
```

| Argument            | Required | Description                                                          |
| ------------------- | -------- | -------------------------------------------------------------------- |
| `KEY`               | Yes      | Secret key name (e.g., `STRIPE_SECRET_KEY`)                          |
| `--env`             | Yes      | Target environment                                                   |
| `--value`           | No       | Secret value. If omitted, prompts interactively (hidden input)       |
| `--group`           | No       | Config group to register the secret under (skips interactive prompt) |
| `--no-sync`         | No       | Store only — skip auto-push to remotes and auto-deploy               |
| `--bail`            | No       | Fail if any remote push fails and skip deploy                        |
| `--skip-validation` | No       | Bypass `validate:` checks on the value                               |
| `--force`           | No       | Skip empty-value confirmation prompt                                 |

**Behavior:**

1. Validates the environment exists in config.
2. If the key isn't in `esk.yaml`:
   - With `--group`: adds the secret to that group in `esk.yaml` non-interactively.
   - Interactive mode (TTY, no `--group`): prompts "Add it?" with a group selector (existing groups or new).
   - Non-interactive mode (piped stdin, no `--group`): warns but proceeds.
3. If the secret has a `validate:` block, checks the value against constraints. Fails unless `--skip-validation` is passed.
4. If the value is empty or whitespace-only (and the secret doesn't have `allow_empty: true`): in TTY mode, prompts for confirmation; in non-TTY mode, warns. Use `--force` to skip the prompt.
5. Stores the value in the encrypted store, incrementing the version counter.
6. Unless `--no-sync`: auto-pushes the environment's secrets to all configured remotes.
7. Unless `--no-sync`: runs `deploy` for the affected environment.
8. With `--bail`: if any remote push fails, exits with an error and skips deploy entirely.
9. Without `--bail`: deploy still runs, but the command exits non-zero if any remote push failed (to surface retry work).

**Examples:**

```bash
esk set API_KEY --env dev                        # Interactive prompt for value
esk set API_KEY --env dev --value sk_test_123    # Inline value
esk set API_KEY --env dev --group Stripe          # Register under Stripe group
esk set API_KEY --env dev --no-sync              # Store only, skip sync and deploy
esk set API_KEY --env dev --bail                 # Fail hard on remote errors
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
- Per-cell status indicators reflect deploy state across configured targets for that key/environment:
  - `✔` (green) — deployed: all targets up to date.
  - `●` (yellow) — pending: value changed since last deploy.
  - `✗` (red) — failed: last deploy attempt failed.
  - `○` (dim) — unset: key is targeted for this environment but has no stored value.
  - Blank — not targeted: key has no configured targets for this environment.
- Keys in the store but not in config appear under "Uncategorized (not in esk.yaml)".

**Example output:**

```
  Stripe
                       dev  prod
  STRIPE_SECRET_KEY     ✔    ●
  STRIPE_WEBHOOK_SECRET ✔

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

| Argument | Required | Description                              |
| -------- | -------- | ---------------------------------------- |
| `--env`  | No       | Filter to a single environment           |
| `--all`  | No       | Show all targets including deployed ones |

Displays a multi-section dashboard with the following sections:

- **Summary** — Project name, store version, and target counts with status breakdown.
- **Targets** — Target health from preflight checks (pass/fail per target).
- **Deploy (targets)** — Secrets grouped by status: failed, pending, unset, and deployed (deployed hidden unless `--all`). Entries include relative deploy freshness (for example, "3h ago") and error details for failures.
- **Coverage** — Gaps where a secret is set in some environments but not others, and orphaned secrets (in store but not in config).
- **Sync (remotes)** — Push state per (remote, environment): current, stale (version behind), failed, or never synced.
- **Next steps** — Actionable commands to fix issues (retry failed deploys, deploy pending changes, fill coverage gaps, sync stale remotes, remove orphans).

The dashboard closes with the current store version.

**Example output:**

```
  myapp · v5 · 6 targets (3 deployed, 2 pending, 1 unset)

  Targets
    ✓ env            writable
    ✓ cloudflare     wrangler authenticated

  Deploy (targets)
    ● 2 pending
       STRIPE_SECRET_KEY:prod  → cloudflare:web  last deployed 3h ago
       API_KEY:dev  → env:web  never deployed
    ○ 1 unset
       DATABASE_URL:dev  → env:web:dev
    ✓ 3 deployed  (--all to show)

  Next steps
    esk deploy --env prod  deploy 1 pending change
    esk deploy --env dev   deploy 1 pending change
    esk set DATABASE_URL --env dev  fill coverage gap

  Store version: 5
```

---

## `esk generate`

Generate TypeScript declarations (or a runtime validator) for configured secret keys.

```bash
esk generate [--runtime] [--output <PATH>]
```

| Argument          | Required | Description                                                         |
| ----------------- | -------- | ------------------------------------------------------------------- |
| `--runtime`       | No       | Generate `env.ts` runtime validator instead of `.d.ts` declarations |
| `--output` / `-o` | No       | Output path (defaults to `env.d.ts` or `env.ts`)                    |

**Behavior:**

1. Collects unique secret keys from the `secrets` section in `esk.yaml`.
2. Writes `env.d.ts` by default with `NodeJS.ProcessEnv` declarations.
3. With `--runtime`, writes `env.ts` containing a `requireEnv` helper and typed `env` object.
4. Creates parent directories for the output path if needed.
5. Warns when no secrets are defined, and suggests adding the output path to `.gitignore` if it isn't already listed.

**Examples:**

```bash
esk generate
esk generate --runtime
esk generate --runtime --output src/env.ts
```

---

## `esk sync`

Sync secrets with configured remotes. Pulls from remotes, reconciles with the local store, then pushes merged data to stale or drifted remotes.

```bash
esk sync [--env <ENV>] [--only <REMOTE>] [--dry-run] [--bail] [--force] [--with-deploy] [--prefer <local|remote>]
```

| Argument        | Required | Description                                                          |
| --------------- | -------- | -------------------------------------------------------------------- |
| `--env`         | No       | Environment to sync (omit to sync all configured environments)       |
| `--only`        | No       | Sync a specific remote only                                          |
| `--dry-run`     | No       | Show what would change without modifying anything                    |
| `--bail`        | No       | Fail on first error (remote pull failure or per-environment failure) |
| `--force`       | No       | Bypass version jump protection (use with caution)                    |
| `--with-deploy` | No       | Auto-run `deploy` after syncing                                      |
| `--prefer`      | No       | Conflict preference at equal version (`local` default, or `remote`)  |

**Requires:** At least one remote configured in `esk.yaml`. Remotes that fail preflight are skipped; if none remain, sync exits with a warning and no changes.

**Behavior:**

1. Syncs the selected environment(s): `--env` limits to one; omitted means all configured environments.
2. Pulls secrets and versions from all available remotes (or just `--only <name>`).
3. Uses the highest version as the base and merges unique keys from lower versions.
4. Updates local store state when reconciliation changes it.
5. Pushes merged/current data to stale remotes, including equal-version drift repair (no interactive push prompt).
6. With `--bail`: aborts on the first remote pull failure or the first environment sync failure. Without `--bail`: logs failing environments and continues; exits non-zero if any failed.
7. With `--with-deploy`, runs `esk deploy --env <ENV>` only for environments where local store state changed.
8. With `--dry-run`, shows what would change without modifying store or remote state.

**Examples:**

```bash
esk sync                                # Sync all environments and remotes
esk sync --env prod                     # Sync one environment
esk sync --env prod --only 1password    # Sync specific remote
esk sync --env prod --with-deploy       # Sync + auto-deploy
esk sync --env prod --prefer remote     # At equal versions, prefer remote content
esk sync --env prod --dry-run           # Preview changes
```

---

## Secret definitions

Each secret in `esk.yaml` supports the following fields:

```yaml
secrets:
  Payments:
    STRIPE_KEY:
      description: Stripe API key
      targets:
        cloudflare: [web:prod]
        env: [web:dev]
      validate:
        format: string
        min_length: 7
        pattern: "^sk_(test|live)_"
      required: true
      allow_empty: false
```

### `validate:`

Optional block that checks values at `esk set` and before `esk deploy`. Bypass with `--skip-validation`.

| Field        | Type            | Description                                                     |
| ------------ | --------------- | --------------------------------------------------------------- |
| `format`     | string          | Value format: `string`, `url`, `integer`, `number`, `boolean`, `email`, `json`, `base64` |
| `enum`       | list            | Allowed values (exact match)                                    |
| `pattern`    | string          | Regex the value must match                                      |
| `min_length` | integer         | Minimum character length                                        |
| `max_length` | integer         | Maximum character length                                        |
| `range`      | [min, max]      | Numeric range (requires `format: integer` or `number`)          |
| `optional`   | boolean         | If `true`, empty values skip all other checks (default `false`) |

Cross-field constraints (evaluated at deploy with the full store context):

| Field             | Type          | Description                                                    |
| ----------------- | ------------- | -------------------------------------------------------------- |
| `required_if`     | map           | Required when all listed keys match their values (`"*"` = any) |
| `required_with`   | list          | Required when any listed key has a value                       |
| `required_unless` | list          | Not required when any listed key has a value                   |

### `required:`

Controls whether deploy fails when the secret has no stored value. Default: `true`.

| Value              | Meaning                                         |
| ------------------ | ----------------------------------------------- |
| `true`             | Required in all targeted environments (default) |
| `false`            | Never required                                  |
| `[dev, prod]`      | Required only in listed environments            |

Bypass with `--skip-requirements` or `--force` on deploy. `esk delete` warns interactively when removing a required secret.

### `allow_empty:`

Boolean, default `false`. When `true`, the secret is exempt from empty-value warnings and blocks in `set`, `deploy`, `status`, and `sync`. Useful for secrets that legitimately have empty values (feature flags, optional overrides).

---

## Files

| File                     | Description                               | Commit to git?  |
| ------------------------ | ----------------------------------------- | --------------- |
| `esk.yaml`               | Project configuration                     | Yes             |
| `.esk/store.enc`         | AES-256-GCM encrypted secret store        | Yes             |
| `.esk/store.key`         | 32-byte encryption key (hex)              | **No**          |
| `.esk/deploy-index.json` | Deploy state (hashes, timestamps, status) | No (gitignored) |
| `.esk/sync-index.json`   | Sync state (versions, timestamps)         | No (gitignored) |

## Exit codes

| Code | Meaning                                                           |
| ---- | ----------------------------------------------------------------- |
| `0`  | Success                                                           |
| `1`  | Error (missing config, unknown environment, deploy failure, etc.) |
