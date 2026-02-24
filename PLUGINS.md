# Plugins

Plugins sync the entire secret list via `esk sync`. Unlike adapters (which deploy individual secrets to targets), plugins operate on the full store per environment — pulling from remote, reconciling, and pushing merged results back.

For deploy adapters (env files, Cloudflare, Convex, etc.), see [ADAPTERS.md](ADAPTERS.md).

## Overview

| Plugin                                            | Config key              | External CLI | Storage location              |
| ------------------------------------------------- | ----------------------- | ------------ | ----------------------------- |
| [1Password](#1password)                           | `1password`             | `op`         | 1Password vault item          |
| [Cloud file](#cloud-file)                         | Any name + `type: cloud_file` | None   | Local/cloud-synced folder     |
| [AWS Secrets Manager](#aws-secrets-manager)       | `aws_secrets_manager`   | `aws`        | AWS Secrets Manager           |
| [HashiCorp Vault](#hashicorp-vault)               | `vault`                 | `vault`      | Vault KV store                |
| [Bitwarden](#bitwarden)                           | `bitwarden`             | `bws`        | Bitwarden Secrets Manager     |
| [S3](#s3)                                         | `s3`                    | `aws`        | S3-compatible bucket          |
| [GCP Secret Manager](#gcp-secret-manager)         | `gcp`                   | `gcloud`     | GCP Secret Manager            |
| [Azure Key Vault](#azure-key-vault)               | `azure`                 | `az`         | Azure Key Vault               |
| [Doppler](#doppler)                               | `doppler`               | `doppler`    | Doppler project               |
| [SOPS](#sops)                                     | `sops`                  | `sops`       | SOPS-encrypted files          |

---

## 1Password

Uses the 1Password CLI (`op`) to push and pull entire environment snapshots as vault items.

### How it works

**Push** (during `esk sync` or auto-push from `esk set`/`delete`):

1. Collects all secrets for the environment from the local store.
2. Groups them by vendor (using the `secrets` section of `esk.yaml`).
3. Creates or updates a 1Password item with concealed fields organized into vendor sections. On update, fields present in 1Password but absent from the local store are deleted using `[delete]` field assignments.
4. Stores a `_Metadata.version` field for reconciliation.

**Pull** (during `esk sync`):

1. Fetches the 1Password item for the environment.
2. Parses fields back into key-value pairs (section label = vendor, field label = key).
3. Reads the version from `_Metadata`.
4. Reconciles with the local store and other plugins using version comparison.

**Auto-push**: The `set` and `delete` commands automatically push to all configured plugins after modifying the store (unless `--no-sync` is used).

### Prerequisites

- [1Password CLI](https://developer.1password.com/docs/cli/) (`op`) installed and authenticated.
- A vault accessible to the authenticated user.

Preflight verifies both CLI installation and vault accessibility by running `op vault get <vault>`.

### Configuration

```yaml
plugins:
  1password:
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
2. Bob syncs from 1Password — his local store is at version 3, remote is 5.
3. Remote wins: Bob's store is updated with remote secrets. Any keys Bob has that Alice doesn't are merged in.
4. If Bob's local version were higher, his secrets win and the merged result is pushed back.

---

## Cloud file

Stores the secret payload in a local or cloud-synced folder (Dropbox, Google Drive, OneDrive, or any folder). The cloud sync itself is handled by the respective desktop app — esk just reads and writes files in the configured path.

### How it works

Files are stored per environment (`secrets-{env}.enc` or `secrets-{env}.json`), so each environment is isolated. Only secrets for the pushed environment are included — no cross-environment leakage.

**Encrypted format** (`format: encrypted`):

- **Push**: Encrypts the environment's secrets and writes to `{path}/secrets-{env}.enc`.
- **Pull**: Reads `{path}/secrets-{env}.enc`, decrypts with the local `.esk/store.key`, returns the payload.
- The cloud copy is encrypted — it's safe to store in shared or less-trusted locations.

**Cleartext format** (`format: cleartext`):

- **Push**: Writes `{path}/secrets-{env}.json` containing the environment's secrets as JSON.
- **Pull**: Reads `{path}/secrets-{env}.json`, parses secrets and version.
- Useful when you want human-readable backup or when other tools need to consume the secrets.

Both formats use atomic writes (temp file + rename) and create parent directories automatically.

Preflight checks that the configured path is an existing, writable directory. Fails if the path doesn't exist or isn't writable.

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

- `{project}` is replaced with the project name from `esk.yaml`
- `~` is expanded to the `$HOME` environment variable

Examples:

- `~/Dropbox/esk/{project}` → `/Users/alice/Dropbox/esk/myapp`
- `~/Dropbox/secrets/myproject` → `/Users/alice/Dropbox/secrets/myproject`
- `/absolute/path` → unchanged

---

## AWS Secrets Manager

Uses the AWS CLI to store and retrieve entire environment snapshots as JSON in AWS Secrets Manager.

### How it works

**Push**: Serializes the environment's secrets and version as JSON, then stores it as the secret value via `--secret-string file:///dev/stdin` (JSON piped via stdin to avoid exposing secrets in process arguments). Creates the secret on first push; updates it on subsequent pushes.

**Pull**: Retrieves the secret value via `aws secretsmanager get-secret-value`, parses the JSON, and returns composite keys with the version.

### Prerequisites

- [AWS CLI](https://aws.amazon.com/cli/) installed and authenticated (`aws configure`).

Preflight runs `aws sts get-caller-identity` to verify credentials and connectivity.

### Configuration

```yaml
plugins:
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
    region: us-west-2
    profile: staging
```

| Field         | Required | Description                                                                                 |
| ------------- | -------- | ------------------------------------------------------------------------------------------- |
| `secret_name` | Yes      | Secret name pattern. Supports `{project}` and `{environment}` interpolation.                |
| `region`      | No       | AWS region. Passed as `--region` flag.                                                      |
| `profile`     | No       | AWS profile. Passed as `--profile` flag.                                                    |

### Secret naming

| Pattern                     | Project | Environment | Result         |
| --------------------------- | ------- | ----------- | -------------- |
| `{project}/{environment}`   | `myapp` | `dev`       | `myapp/dev`    |
| `{project}/{environment}`   | `myapp` | `prod`      | `myapp/prod`   |

---

## HashiCorp Vault

Uses the Vault CLI to push and pull environment snapshots to a Vault KV store. Supports both KV v1 and KV v2 secret engines.

### How it works

**Push**: Writes secrets as key-value pairs (plus `_esk_version` metadata) to a Vault KV path via `vault kv put`. The JSON is piped via stdin.

**Pull**: Reads from the KV path via `vault kv get -format=json`. Handles the different JSON structures of KV v1 (`.data`) and KV v2 (`.data.data`).

### Prerequisites

- [Vault CLI](https://developer.hashicorp.com/vault/install) installed and authenticated (`vault login`).

Preflight runs `vault token lookup` to verify the current token is valid.

### Configuration

```yaml
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
    addr: "https://vault.example.com"
    kv_version: 2
```

| Field        | Required | Default | Description                                                                                 |
| ------------ | -------- | ------- | ------------------------------------------------------------------------------------------- |
| `path`       | Yes      | —       | KV path pattern. Supports `{project}` and `{environment}` interpolation.                    |
| `addr`       | No       | —       | Vault server address. Set as `VAULT_ADDR` environment variable.                             |
| `kv_version` | No       | `2`     | KV secret engine version (`1` or `2`). Determines the JSON response parsing path.           |

### Command executed

```bash
# Push (stdin contains JSON with secrets + _esk_version):
vault kv put secret/data/myapp/dev -

# Pull:
vault kv get -format=json secret/data/myapp/dev
```

---

## Bitwarden

Uses the Bitwarden Secrets Manager CLI (`bws`) to store environment snapshots as JSON-valued secrets in a Bitwarden project.

### How it works

**Push**: Lists existing secrets in the project, then either creates or updates the secret matching the resolved name. The value is a JSON object with bare keys and `_esk_version`.

**Pull**: Lists secrets, finds the one matching the name, and parses its JSON value.

### Prerequisites

- [Bitwarden Secrets Manager CLI](https://bitwarden.com/help/secrets-manager-cli/) (`bws`) installed.
- `BWS_ACCESS_TOKEN` environment variable set.

Preflight runs `bws secret list --project-id <id>` to verify `BWS_ACCESS_TOKEN` is set and valid.

### Configuration

```yaml
plugins:
  bitwarden:
    project_id: "proj-123-abc"
    secret_name: "{project}-{environment}"
```

| Field         | Required | Description                                                                                 |
| ------------- | -------- | ------------------------------------------------------------------------------------------- |
| `project_id`  | Yes      | Bitwarden Secrets Manager project ID.                                                       |
| `secret_name` | Yes      | Secret name pattern. Supports `{project}` and `{environment}` interpolation.                |

### Command executed

```bash
# List secrets:
bws secret list --project-id <id> --output json

# Create:
bws secret create <name> <json-value> --project-id <id>

# Update:
bws secret edit <secret-id> --value <json-value>
```

---

## S3

Stores encrypted or cleartext files in any S3-compatible bucket (AWS S3, Cloudflare R2, MinIO, DigitalOcean Spaces). Uses the same file format as the cloud file plugin.

### How it works

Files are stored per environment (`secrets-{env}.enc` or `secrets-{env}.json`). Content is piped via stdin to `aws s3 cp - <uri>` for upload and from `aws s3 cp <uri> -` for download.

**Encrypted format**: Uses the same AES-256-GCM encryption as the local store. Requires the `.esk/store.key` for push and pull.

**Cleartext format**: Stores a JSON payload with bare keys and version.

### Prerequisites

- [AWS CLI](https://aws.amazon.com/cli/) installed and authenticated (`aws configure`).

Preflight runs `aws sts get-caller-identity` to verify credentials and connectivity.

### Configuration

```yaml
plugins:
  s3:
    bucket: my-secrets-bucket
    prefix: esk/myapp
    format: encrypted
    region: us-west-2
    profile: myprofile
    endpoint: "https://r2.example.com"
```

| Field      | Required | Default     | Description                                                                                 |
| ---------- | -------- | ----------- | ------------------------------------------------------------------------------------------- |
| `bucket`   | Yes      | —           | S3 bucket name.                                                                             |
| `prefix`   | No       | —           | Key prefix within the bucket (e.g., `esk/myapp`).                                           |
| `format`   | No       | `encrypted` | Storage format: `encrypted` (needs key) or `cleartext` (JSON).                              |
| `region`   | No       | —           | AWS region. Passed as `--region`.                                                            |
| `profile`  | No       | —           | AWS profile. Passed as `--profile`.                                                          |
| `endpoint` | No       | —           | Custom endpoint URL for S3-compatible services. Passed as `--endpoint-url`.                  |

### File layout

```
s3://my-secrets-bucket/esk/myapp/
  secrets-dev.enc    # Encrypted, dev environment
  secrets-prod.enc   # Encrypted, prod environment
```

### Command executed

```bash
# Upload:
echo "<content>" | aws s3 cp - s3://bucket/prefix/secrets-dev.enc [--region ...] [--profile ...] [--endpoint-url ...]

# Download:
aws s3 cp s3://bucket/prefix/secrets-dev.enc - [--region ...] [--profile ...] [--endpoint-url ...]
```

---

## GCP Secret Manager

Uses the `gcloud` CLI to store and retrieve environment snapshots as JSON in GCP Secret Manager.

### How it works

**Push**: Adds a new version to the GCP secret via `gcloud secrets versions add`. Creates the secret automatically on first push if it doesn't exist.

**Pull**: Accesses the latest version via `gcloud secrets versions access latest`. The JSON contains bare keys plus `_esk_version`.

### Prerequisites

- [Google Cloud CLI](https://cloud.google.com/sdk/docs/install) (`gcloud`) installed and authenticated.

Preflight runs `gcloud auth print-access-token --project <gcp_project>` to verify authentication and project access.

### Configuration

```yaml
plugins:
  gcp:
    gcp_project: my-gcp-project
    secret_name: "{project}-{environment}"
```

| Field         | Required | Description                                                                                 |
| ------------- | -------- | ------------------------------------------------------------------------------------------- |
| `gcp_project` | Yes      | GCP project ID.                                                                             |
| `secret_name` | Yes      | Secret name pattern. Supports `{project}` and `{environment}` interpolation.                |

### Command executed

```bash
# Push (stdin contains JSON):
gcloud secrets versions add myapp-dev --data-file=- --project my-gcp-project

# Pull:
gcloud secrets versions access latest --secret=myapp-dev --project my-gcp-project
```

---

## Azure Key Vault

Uses the Azure CLI to store and retrieve environment snapshots as JSON in Azure Key Vault.

### How it works

**Push**: Stores all secrets for the environment as a single JSON-valued Key Vault secret via `az keyvault secret set`.

**Pull**: Retrieves the secret via `az keyvault secret show`, parses the JSON from the `.value` field.

Secret names are automatically sanitized — non-alphanumeric characters (except hyphens) are replaced with hyphens, since Azure Key Vault secret names only allow alphanumeric characters and hyphens.

### Prerequisites

- [Azure CLI](https://learn.microsoft.com/en-us/cli/azure/install-azure-cli) (`az`) installed and authenticated (`az login`).

Preflight runs `az account show` to verify the CLI is authenticated.

### Configuration

```yaml
plugins:
  azure:
    vault_name: my-vault
    secret_name: "{project}-{environment}"
```

| Field         | Required | Description                                                                                 |
| ------------- | -------- | ------------------------------------------------------------------------------------------- |
| `vault_name`  | Yes      | Azure Key Vault name.                                                                       |
| `secret_name` | Yes      | Secret name pattern. Supports `{project}` and `{environment}` interpolation. Sanitized to alphanumeric + hyphens. |

### Command executed

```bash
# Push (JSON written to temp file to avoid process argument exposure):
az keyvault secret set --vault-name my-vault --name myapp-dev --file /tmp/esk-XXXXXX

# Pull:
az keyvault secret show --vault-name my-vault --name myapp-dev --output json
```

---

## Doppler

Uses the Doppler CLI to sync secrets to Doppler projects. Each esk environment maps to a Doppler config via `config_map`.

### How it works

**Push**: Uploads all secrets for the environment as a single JSON payload via `doppler secrets upload --json` (piped via stdin), plus a `_esk_version` metadata key for reconciliation.

**Pull**: Downloads all secrets via `doppler secrets download --format json`.

### Prerequisites

- [Doppler CLI](https://docs.doppler.com/docs/install-cli) installed and authenticated (`doppler login`).

Preflight runs `doppler me` to verify the CLI is authenticated.

### Configuration

```yaml
plugins:
  doppler:
    project: myapp-doppler
    config_map:
      dev: dev_config
      prod: prd
```

| Field        | Required | Description                                                                                 |
| ------------ | -------- | ------------------------------------------------------------------------------------------- |
| `project`    | Yes      | Doppler project name.                                                                       |
| `config_map` | Yes      | Maps esk environment names to Doppler config names.                                         |

### Command executed

```bash
# Push (all secrets as JSON piped via stdin):
doppler secrets upload --json -p myapp-doppler -c dev_config --silent

# Pull:
doppler secrets download -p myapp-doppler -c dev_config --format json --no-file
```

---

## SOPS

Uses [Mozilla SOPS](https://github.com/getsops/sops) to store secrets as encrypted files. SOPS handles the encryption envelope (using your configured KMS, age, or PGP keys), while esk manages the secret content and versioning.

### How it works

**Push**: Serializes secrets as JSON, encrypts via `sops -e /dev/stdin`, and writes the encrypted output to the resolved path atomically.

**Pull**: Decrypts the file via `sops -d <path>` and parses the JSON.

Files are stored per environment using the `{environment}` placeholder in the path.

### Prerequisites

- [SOPS](https://github.com/getsops/sops) installed.
- SOPS configured with a `.sops.yaml` creation rule or appropriate KMS/age/PGP keys.

Preflight checks that a `.sops.yaml` file exists in the project root — fails immediately if absent rather than surfacing an error at push time.

### Configuration

```yaml
plugins:
  sops:
    path: "secrets/{environment}.enc.json"
```

| Field  | Required | Description                                                                                 |
| ------ | -------- | ------------------------------------------------------------------------------------------- |
| `path` | Yes      | File path pattern. Supports `{environment}` interpolation. Relative to project root.        |

### File layout

```
secrets/
  dev.enc.json    # SOPS-encrypted, dev environment
  prod.enc.json   # SOPS-encrypted, prod environment
```

### Command executed

```bash
# Push (stdin → encrypt → write to file):
echo '<json>' | sops -e /dev/stdin > secrets/dev.enc.json

# Pull:
sops -d secrets/dev.enc.json
```

---

## Multi-plugin reconciliation

When multiple plugins are configured, `esk sync` reconciles across all of them:

1. Pull from every configured plugin (or just `--only <name>`).
2. Find the source with the highest version (including local).
3. Start with that as the base.
4. Merge unique secrets from lower-version sources.
5. Write the merged result to the local store.
6. Push the merged result back to any plugins that were behind.

This means you can use 1Password for team sharing and Dropbox as a backup — sync keeps them all in sync.

### Targeting a specific plugin

Use `--only` to sync with a single plugin:

```bash
esk sync --env prod --only 1password
esk sync --env dev --only dropbox
```
