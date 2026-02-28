# esk — encrypted secrets management CLI

esk manages encrypted secrets locally and deploys them to cloud services (Cloudflare, Vercel, Fly, AWS, etc.) and syncs them with remote backends (1Password, S3, Vault, etc.). Secrets are stored AES-256-GCM encrypted on disk, tracked per-environment, and deployed via external CLIs.

## Commands

### `esk init`

Initialize a new esk project in the current directory. Creates `esk.yaml`, `.esk/store.enc`, and `.esk/store.key`.

| Flag | Description |
|------|-------------|
| `--keychain` | Store encryption key in OS keychain instead of file (requires `keychain` feature) |

### `esk set <KEY> --env <ENV>`

Set a secret value. Prompts interactively for the value unless `--value` is provided.

| Flag | Description |
|------|-------------|
| `--env <ENV>` | Environment (required) |
| `--value <VAL>` | Secret value (visible in process list; omit for interactive prompt) |
| `--group <GROUP>` | Config group to register the secret under (skips interactive prompt) |
| `--no-sync` | Skip auto-sync after setting |
| `--bail` | Fail if any remote push fails (skip target deploy) |
| `--skip-validation` | Skip value validation |
| `--force` | Bypass interactive confirmations (empty value, etc.) |

After setting, esk automatically pushes to configured remotes and deploys to configured targets (unless `--no-sync`).

### `esk get <KEY> --env <ENV>`

Retrieve and print a secret value.

| Flag | Description |
|------|-------------|
| `--env <ENV>` | Environment (required) |

### `esk delete <KEY> --env <ENV>`

Delete a secret value from the store.

| Flag | Description |
|------|-------------|
| `--env <ENV>` | Environment (required) |
| `--no-sync` | Skip auto-sync after deleting |
| `--bail` | Fail if any remote push fails (skip target deploy) |

### `esk list`

List all secrets and their status across environments.

| Flag | Description |
|------|-------------|
| `--env <ENV>` | Filter by environment |

### `esk deploy`

Deploy secrets to configured targets. Only deploys secrets whose values have changed (SHA-256 change detection).

| Flag | Description |
|------|-------------|
| `--env <ENV>` | Filter by environment |
| `--force` | Deploy even if hashes match |
| `--dry-run` | Show what would be deployed without deploying |
| `--verbose`, `-v` | Show detailed output |
| `--skip-validation` | Skip value validation |
| `--skip-requirements` | Skip required-secret checks |
| `--allow-empty` | Allow deploying empty/whitespace-only values |
| `--prune` | Remove orphaned secrets from targets (deployed but no longer in config) |

### `esk status`

Show deploy and sync status — which secrets need deploying and which remotes are out of sync.

| Flag | Description |
|------|-------------|
| `--env <ENV>` | Filter by environment |
| `--all` | Show all targets including already-deployed ones |

### `esk sync`

Sync secrets with remotes. Pulls from remote, reconciles with local store, pushes merged results back. Bidirectional.

| Flag | Description |
|------|-------------|
| `--env <ENV>` | Environment to sync (omit to sync all) |
| `--only <REMOTE>` | Sync a specific remote only |
| `--dry-run` | Show what would change without modifying anything |
| `--bail` | Fail if any remote is unreachable (no partial reconciliation) |
| `--force` | Bypass version jump protection |
| `--with-deploy` | Auto-deploy targets after syncing |
| `--prefer <SIDE>` | When versions match but content differs: `local` (default) or `remote` |

### `esk generate [FORMAT]`

Generate code or config files from secret definitions. Runs all configured outputs if no format is specified.

| Flag | Description |
|------|-------------|
| `FORMAT` | `dts` (TypeScript declarations), `ts` (runtime module), or `env-example` |
| `--output`, `-o` | Output file path (requires a format argument) |
| `--preview` | Print generated output to stdout without writing files |

## Config reference (`esk.yaml`)

```yaml
project: my-app                    # Project name (used in path interpolation)
environments: [dev, staging, prod] # Arbitrary environment names

apps:                              # Optional, for per-app target deploy
  web:
    path: apps/web
  api:
    path: apps/api

targets:                           # Deploy targets (esk deploy)
  env:                             # .env file generation (batch)
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
      prod: ".production"
  cloudflare:                      # Cloudflare Workers/Pages
    mode: workers                  # "workers" (default) or "pages"
    pages_project: my-site         # Required when mode is "pages"
    env_flags:
      prod: "--env production"
  convex:                          # Convex
    path: apps/web
    deployment_source: apps/web/.env.local
    env_flags:
      prod: "--prod"
  fly:                             # Fly.io
    app_names:
      web: my-app-web
      api: my-app-api
    env_flags: {}
  netlify:                         # Netlify
    site: my-site
    env_flags: {}
  vercel:                          # Vercel
    env_names:
      dev: development
      prod: production
    env_flags: {}
  github:                          # GitHub Actions secrets
    repo: owner/repo
    env_flags: {}
  heroku:                          # Heroku
    app_names:
      web: my-app-web
    env_flags: {}
  supabase:                        # Supabase
    project_ref: abcdefghijklmnop
    env_flags: {}
  railway:                         # Railway
    env_flags: {}
  gitlab:                          # GitLab CI/CD
    env_flags: {}
  aws_ssm:                         # AWS SSM Parameter Store
    path_prefix: "/{project}/{environment}/"
    region: us-east-1
    profile: my-profile
    parameter_type: SecureString   # SecureString, String, or StringList
    env_flags: {}
  kubernetes:                      # Kubernetes secrets (batch)
    namespace:
      dev: app-dev
      prod: app-prod
    secret_name: my-secrets
    context:
      dev: docker-desktop
      prod: prod-cluster
    env_flags: {}
  docker:                          # Docker Swarm secrets
    name_pattern: "{project}-{environment}-{key}"
    labels:
      managed-by: esk
    env_flags: {}

remotes:                           # Sync remotes (esk sync)
  1password:
    vault: Engineering
    item_pattern: "{project} - {Environment}"
  dropbox:
    type: cloud_file
    path: ~/Dropbox/secrets/{project}
    format: encrypted              # encrypted (default) or cleartext
  vault:                           # HashiCorp Vault
    path: secret/data/{project}/{environment}
    addr: https://vault.example.com:8200
    kv_version: 2
  bitwarden:
    project_id: uuid-here
    secret_name: "{project}-{environment}"
  s3:                              # S3-compatible (AWS, R2, MinIO)
    bucket: my-bucket
    prefix: esk-backups
    endpoint: https://minio.example.com
    region: us-east-1
    profile: my-profile
    format: encrypted
  gcp:                             # GCP Secret Manager
    project: my-gcp-project
    secret_name: "{project}-{environment}"
  azure:                           # Azure Key Vault
    vault_name: my-vault
    secret_name: "{project}-{environment}"
  doppler:
    project: my-app
    config_map:
      dev: dev
      prod: prd
  sops:                            # Mozilla SOPS
    path: secrets/{environment}.enc.yaml
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
    region: us-east-1
    profile: my-profile

generate:                          # Code generation outputs
  - format: dts
    output: types/env.d.ts
  - format: ts
  - format: env-example

secrets:                           # Secrets grouped by category
  GroupName:
    SECRET_KEY:
      description: Human-readable description
      targets:                     # Which targets to deploy to
        env: [web:dev, web:prod]   # app:env format
        cloudflare: [dev, prod]    # env-only when no apps needed
      validate:                    # Value validation (checked at set + deploy)
        format: url                # string, url, integer, number, boolean, email, json, base64
        enum: [a, b, c]           # Allowed values
        pattern: "^sk_"            # Regex pattern
        min_length: 32
        max_length: 256
        range: [1, 65535]          # Numeric range [min, max]
        optional: true             # Allow empty values to pass validation
        required_if:               # Cross-field: required when conditions match
          OTHER_KEY: "true"        # key: value ("*" = any non-empty value)
        required_with: [PAIR_KEY]  # Required when listed secrets have values
        required_unless: [ALT_KEY] # Not required when listed secrets have values
      required: true               # true (default) | false | [env1, env2]
      allow_empty: false           # Reject whitespace-only values (default: false)
```

## Key concepts

**Targets vs remotes**: Targets deploy individual secrets to services (`esk deploy`). Remotes sync the entire store bidirectionally (`esk sync`) for team sharing and backup.

**Environments**: Arbitrary names defined in config (dev, staging, prod, preview, etc.). Not hardcoded.

**Apps**: Optional grouping for per-app deploys. Secret targets use `app:env` format (e.g., `web:prod`) or just `env` when no app scoping is needed.

**Secret groups**: Secrets are organized under named groups in config (e.g., Database, Auth, APIs). Groups are purely organizational.

**Change detection**: SHA-256 hash per (secret, target, app, environment). `esk deploy` skips secrets whose hash hasn't changed. Use `--force` to override.

**Auto-sync**: `esk set` and `esk delete` automatically push to remotes and deploy to targets. Use `--no-sync` to skip.

**Validation**: Checked at `set` time and before `deploy`. Formats: string, url, integer, number, boolean, email, json, base64. Also supports enum, pattern (regex), length, and range. Use `--skip-validation` to bypass.

**Requirements**: `required: true` (default) means the secret must have a value in all targeted environments before deploy. `required: [prod]` limits to specific environments. `required: false` disables. Use `--skip-requirements` or `--force` to bypass.

**Reconciliation**: Version-counter-based. Each store modification increments the version. When syncing, higher version wins. Equal-version conflicts default to local (override with `--prefer remote`).

## Common workflows

**Initialize a project:**
```sh
esk init
# Edit esk.yaml to configure environments, targets, remotes, and secrets
```

**Set secrets:**
```sh
esk set DATABASE_URL --env dev              # Interactive prompt
esk set DATABASE_URL --env dev --value "postgres://..."  # Inline
esk set NEW_KEY --env dev --group APIs      # Auto-register in config group
```

**Deploy to targets:**
```sh
esk deploy --env prod                       # Deploy changed secrets to prod
esk deploy --env prod --force               # Re-deploy everything
esk deploy --dry-run                        # Preview changes
esk deploy --prune                          # Remove orphaned secrets from targets
```

**Check status:**
```sh
esk status                                  # Overview of pending deploys and sync state
esk status --env prod                       # Filter to prod
esk list                                    # List all secrets with values set/missing
```

**Sync with remotes:**
```sh
esk sync                                    # Pull, reconcile, push all environments
esk sync --env prod --only 1password        # Sync prod with 1Password only
esk sync --with-deploy                      # Sync then auto-deploy
```

**Generate code:**
```sh
esk generate dts                            # TypeScript declarations
esk generate ts --output src/env.ts         # Runtime module with custom path
esk generate                                # Run all configured outputs
```

## Tips

- Secrets are encrypted at rest in `.esk/store.enc` (safe to commit). The key `.esk/store.key` must be gitignored.
- The store tracks tombstones and per-environment versions for correct merge behavior.
- Use `esk deploy --dry-run` before deploying to review what will change.
- Batch targets (env, kubernetes) regenerate their full output on any change. Individual targets (cloudflare, vercel, fly, etc.) deploy one secret at a time.
- Cross-field validation rules (`required_if`, `required_with`, `required_unless`) are evaluated at deploy time, not at set time.
