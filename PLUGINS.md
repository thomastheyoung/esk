# Plugins

Plugins store and back up the entire secret list via `lockbox push` and `lockbox pull`. Unlike adapters (which deploy individual secrets to targets), plugins operate on the full store per environment.

For sync adapters (env files, Cloudflare, Convex), see [ADAPTERS.md](ADAPTERS.md).

## Overview

| Plugin                    | Config key                    | External CLI | Storage location          |
| ------------------------- | ----------------------------- | ------------ | ------------------------- |
| [1Password](#1password)   | `onepassword`                 | `op`         | 1Password vault item      |
| [Cloud file](#cloud-file) | Any name + `type: cloud_file` | None         | Local/cloud-synced folder |

---

## 1Password

Uses the 1Password CLI (`op`) to push and pull entire environment snapshots as vault items.

### How it works

**Push** (`lockbox push --env <ENV>`):

1. Collects all secrets for the environment from the local store.
2. Groups them by vendor (using the `secrets` section of `lockbox.yaml`).
3. Creates or updates a 1Password item with concealed fields organized into vendor sections.
4. Stores a `_Metadata.version` field for reconciliation.

**Pull** (`lockbox pull --env <ENV>`):

1. Fetches the 1Password item for the environment.
2. Parses fields back into key-value pairs (section label = vendor, field label = key).
3. Reads the version from `_Metadata`.
4. Reconciles with the local store and other plugins using version comparison.

**Auto-push**: The `set` command automatically pushes to all configured plugins after storing a secret.

### Prerequisites

- [1Password CLI](https://developer.1password.com/docs/cli/) (`op`) installed and authenticated.
- A vault accessible to the authenticated user.

### Configuration

```yaml
plugins:
  onepassword:
    vault: Engineering
    item_pattern: "{project} - {Environment}"
```

| Field          | Required | Description                                                                                                                 |
| -------------- | -------- | --------------------------------------------------------------------------------------------------------------------------- |
| `vault`        | Yes      | 1Password vault name to store items in.                                                                                     |
| `item_pattern` | Yes      | Template for item names. Supports `{project}`, `{Environment}` (capitalized), and `{environment}` (lowercase) placeholders. |

### Item naming

The `item_pattern` is resolved per environment:

| Pattern                     | Project | Environment | Result          |
| --------------------------- | ------- | ----------- | --------------- |
| `{project} - {Environment}` | `myapp` | `dev`       | `myapp - Dev`   |
| `{project} - {Environment}` | `myapp` | `prod`      | `myapp - Prod`  |
| `{project} {environment}`   | `myapp` | `staging`   | `myapp staging` |

### 1Password item structure

Items are created as "Secure Note" category with the following field layout:

```
Title: myapp - Prod
Category: Secure Note
Vault: Engineering

Sections:
  Stripe:
    STRIPE_KEY [concealed] = sk_live_...
    STRIPE_WEBHOOK_SECRET [concealed] = whsec_...
  Auth:
    AUTH_SECRET [concealed] = my-session-key
  _Metadata:
    version [text] = 5
```

Fields use the `[concealed]` type so values are hidden by default in the 1Password UI. The `_Metadata` section is internal and excluded when pulling secrets.

### Version reconciliation

The version field enables conflict-free merging between team members:

1. Alice sets a secret locally (version goes to 5), pushes to 1Password.
2. Bob pulls from 1Password — his local store is at version 3, remote is 5.
3. Remote wins: Bob's store is updated with remote secrets. Any keys Bob has that Alice doesn't are merged in.
4. If Bob's local version were higher, pull would advise him to push instead.

---

## Cloud file

Stores the secret payload in a local or cloud-synced folder (Dropbox, Google Drive, OneDrive, or any folder). The cloud sync itself is handled by the respective desktop app — lockbox just reads and writes files in the configured path.

### How it works

Files are stored per environment (`secrets-{env}.enc` or `secrets-{env}.json`), so each environment is isolated. Only secrets for the pushed environment are included — no cross-environment leakage.

**Encrypted format** (`format: encrypted`):

- **Push**: Encrypts the environment's secrets and writes to `{path}/secrets-{env}.enc`.
- **Pull**: Reads `{path}/secrets-{env}.enc`, decrypts with the local `.lockbox/store.key`, returns the payload.
- The cloud copy is encrypted — it's safe to store in shared or less-trusted locations.

**Cleartext format** (`format: cleartext`):

- **Push**: Writes `{path}/secrets-{env}.json` containing the environment's secrets as JSON.
- **Pull**: Reads `{path}/secrets-{env}.json`, parses secrets and version.
- Useful when you want human-readable backup or when other tools need to consume the secrets.

Both formats use atomic writes (temp file + rename) and create parent directories automatically.

**Backward compatibility**: On pull, if a per-env file doesn't exist, the plugin falls back to the legacy global file (`secrets.enc` or `secrets.json`) and prints a migration warning. On push, legacy global files are automatically removed after the per-env file is written.

### Configuration

Cloud file plugins use any name you choose, with `type: cloud_file` to identify them:

```yaml
plugins:
  dropbox:
    type: cloud_file
    path: ~/Dropbox/secrets/myproject
    format: encrypted
  gdrive:
    type: cloud_file
    path: "~/Google Drive/secrets/myproject"
    format: cleartext
```

| Field    | Required | Default     | Description                                                                                        |
| -------- | -------- | ----------- | -------------------------------------------------------------------------------------------------- |
| `type`   | Yes      | —           | Must be `cloud_file`.                                                                              |
| `path`   | Yes      | —           | Directory to store files in. Supports `{project}` interpolation and tilde (`~`) expansion to `$HOME`. |
| `format` | No       | `encrypted` | Storage format: `encrypted` (binary, needs key) or `cleartext` (JSON).                             |

### File layout

**Encrypted:**

```
~/Dropbox/secrets/myproject/
  secrets-dev.enc    # AES-256-GCM encrypted, dev environment only
  secrets-prod.enc   # AES-256-GCM encrypted, prod environment only
```

**Cleartext:**

```
~/Google Drive/secrets/myproject/
  secrets-dev.json   # { "secrets": { "KEY": "value", ... }, "version": N }
  secrets-prod.json  # { "secrets": { "KEY": "value", ... }, "version": N }
```

Per-env files use bare keys (e.g., `KEY`) rather than composite keys (`KEY:env`), since the environment is encoded in the filename.

### Path interpolation and expansion

- `{project}` is replaced with the project name from `lockbox.yaml`
- `~` is expanded to the `$HOME` environment variable

Examples:

- `~/Dropbox/lockbox/{project}` → `/Users/alice/Dropbox/lockbox/myapp`
- `~/Dropbox/secrets/myproject` → `/Users/alice/Dropbox/secrets/myproject`
- `/absolute/path` → unchanged

---

## Multi-plugin reconciliation

When multiple plugins are configured, `lockbox pull` reconciles across all of them:

1. Pull from every configured plugin (or just `--only <name>`).
2. Find the source with the highest version (including local).
3. Start with that as the base.
4. Merge unique secrets from lower-version sources.
5. Write the merged result to the local store.
6. Push the merged result back to any plugin that was behind.

This means you can use 1Password for team sharing and Dropbox as a backup — pull keeps them all in sync.

### Targeting a specific plugin

Use `--only` to push or pull from a single plugin:

```bash
lockbox push --env prod --only onepassword
lockbox pull --env dev --only dropbox
```
