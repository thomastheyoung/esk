# lockbox

Encrypted secrets management with multi-target sync.

Lockbox stores secrets in an AES-256-GCM encrypted file, tracked in git, and syncs them to the places they need to be: `.env` files, Cloudflare Workers, Convex deployments, 1Password vaults.

## Quick start

```bash
# Initialize in your project
lockbox init

# Set a secret
lockbox set OPENAI_API_KEY --env dev

# Sync to all configured targets
lockbox sync --env dev

# Check what's out of date
lockbox status
```

## How it works

**Encrypted store** — secrets are encrypted with AES-256-GCM and stored in `.secrets.enc` (safe to commit). The encryption key lives in `.secrets.key` (gitignored).

**Config-driven targets** — `lockbox.yaml` declares your secrets, environments, apps, and sync adapters. No hardcoded paths or assumptions about project structure.

**Change detection** — SHA-256 hashing tracks what's been synced where. Only changed secrets hit external APIs.

**1Password backup** — optional push/pull to 1Password for team sharing and disaster recovery.

## Configuration

```yaml
project: myapp

environments: [dev, prod]

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
      prod: ".production"

  cloudflare:
    env_flags:
      dev: ""
      prod: "--env production"

  convex:
    path: packages/backend
    deployment_source: apps/web/.env.local
    env_flags:
      dev: ""
      prod: "--prod"

  onepassword:
    vault: MyVault
    item_pattern: "{project} {Environment} Secrets"

secrets:
  Auth:
    AUTH_SECRET:
      description: Session signing key
      targets:
        env: [web:dev, web:prod, api:dev]
        cloudflare: [web:dev, web:prod]
```

## Commands

| Command | Description |
|---------|-------------|
| `lockbox init` | Initialize encrypted store and config |
| `lockbox set <KEY>` | Set a secret value |
| `lockbox get <KEY>` | Retrieve a secret (debug) |
| `lockbox list` | Show all secrets and their status |
| `lockbox sync` | Sync secrets to configured targets |
| `lockbox status` | Show sync status and drift |
| `lockbox push` | Push to 1Password |
| `lockbox pull` | Pull from 1Password |

All commands accept `--env <name>` to target a specific environment.

## Built-in adapters

- **env** — generates `.env` files with grouped, sorted output
- **cloudflare** — `wrangler secret put` for Cloudflare Workers
- **convex** — `npx convex env set` for Convex deployments
- **onepassword** — team backup/restore via `op` CLI

## License

MIT
