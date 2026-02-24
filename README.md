```
▄▖          ▗    ▌  ▄▖        ▗     ▖▖
▙▖▛▌▛▘▛▘▌▌▛▌▜▘█▌▛▌  ▚ █▌▛▘▛▘█▌▜▘▛▘  ▙▘█▌█▌▛▌█▌▛▘
▙▖▌▌▙▖▌ ▙▌▙▌▐▖▙▖▙▌  ▄▌▙▖▙▖▌ ▙▖▐▖▄▌  ▌▌▙▖▙▖▙▌▙▖▌
        ▄▌▌                               ▌
```

`esk` is an encrypted secrets manager that lets you define secrets once and deploy them to many targets.

It is built for teams that want:

- A local encrypted source of truth
- Simple deploys to local files and cloud platforms
- Optional sync/backup with shared secret backends

## What esk does

- Stores secrets in `.esk/store.enc` (AES-256-GCM encrypted)
- Keeps the decryption key in `.esk/store.key` (local only)
- Deploys to adapters like `.env` files, Cloudflare, Convex, Vercel, GitHub Actions, Kubernetes, and more
- Syncs with plugins like 1Password, cloud folders, AWS Secrets Manager, Vault, Bitwarden, S3, GCP, Azure, Doppler, and SOPS

## Install

**Shell script (Linux/macOS)**

```bash
curl -fsSL https://raw.githubusercontent.com/thomastheyoung/esk/main/install.sh | bash
```

**Cargo**

```bash
cargo install esk
cargo binstall esk
```

**From source**

```bash
git clone https://github.com/thomastheyoung/esk.git
cd esk
cargo build --release
```

## 60-second quick start

1. Initialize a project.

```bash
esk init
```

2. Add your first secret.

```bash
esk set API_KEY --env dev --group General
```

3. Add more secrets without deploying on each write, then deploy once.

```bash
esk set DATABASE_URL --env dev --group General --no-sync
esk deploy --env dev
```

4. Verify status.

```bash
esk list --env dev
esk status --env dev
```

`esk init` creates:

| File                     | Purpose                                                         | Commit to git |
| ------------------------ | --------------------------------------------------------------- | ------------- |
| `esk.yaml`               | Project config (environments, apps, adapters, plugins, secrets) | Yes           |
| `.esk/store.enc`         | Encrypted secret store                                          | Yes           |
| `.esk/store.key`         | Local encryption key (32-byte hex)                              | No            |
| `.esk/sync-index.json`   | Deploy state tracker                                            | Optional      |
| `.esk/plugin-index.json` | Plugin push state tracker                                       | Optional      |

## Mental model

`esk` has 3 parts:

1. **Store**: local encrypted data (`.esk/store.enc` + `.esk/store.key`)
2. **Adapters**: deploy secrets to runtime targets (`esk deploy`)
3. **Plugins**: sync full secret state to team/shared backends (`esk sync`)

## Important default behavior

By default, `esk set` and `esk delete` do more than update local storage:

1. Update encrypted local store
2. Push to configured plugins
3. Deploy to configured adapters

Use `--no-sync` to skip steps 2 and 3. Use `--strict` to fail before deploy if any plugin push fails.

## Minimal config (`esk.yaml`)

Start with local `.env` deploy only:

```yaml
project: myapp

environments: [dev, prod]

apps:
  web:
    path: .

adapters:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
      prod: ".production"

secrets:
  General:
    API_KEY:
      description: Example API key
      targets:
        env: [web:dev, web:prod]
```

When you need cloud targets or shared sync, add adapter/plugin blocks. See [ADAPTERS.md](ADAPTERS.md) and [PLUGINS.md](PLUGINS.md).

## Commands you will use most

| Command                        | Purpose                                       |
| ------------------------------ | --------------------------------------------- |
| `esk init`                     | Initialize config and encrypted store         |
| `esk set <KEY> --env <ENV>`    | Set a secret (auto-sync/deploy by default)    |
| `esk get <KEY> --env <ENV>`    | Read a secret                                 |
| `esk delete <KEY> --env <ENV>` | Delete a secret (auto-sync/deploy by default) |
| `esk list [--env <ENV>]`       | List secrets and deploy status                |
| `esk deploy [--env <ENV>]`     | Deploy to adapter targets                     |
| `esk status [--env <ENV>]`     | Show drift/sync dashboard                     |
| `esk sync --env <ENV>`         | Pull, reconcile, and push plugin state        |

Full flags and behavior: [API.md](API.md).

## Supported deploy adapters

- `env`
- `cloudflare`
- `convex`
- `fly`
- `netlify`
- `vercel`
- `github`
- `heroku`
- `supabase`
- `railway`
- `gitlab`
- `aws_ssm`
- `kubernetes`

Adapter config details: [ADAPTERS.md](ADAPTERS.md).

## Supported sync plugins

- `1password`
- Cloud file (`dropbox`, `gdrive`, `onedrive`, etc.)
- `aws_secrets_manager`
- `vault`
- `bitwarden`
- `s3`
- `gcp`
- `azure`
- `doppler`
- `sops`

Plugin config details: [PLUGINS.md](PLUGINS.md).

## Security model

- Encryption: AES-256-GCM with a random nonce for every write
- Key isolation: `.esk/store.key` stays local and must not be committed
- Tamper resistance: authenticated encryption
- Reliability: atomic writes for store and index files

The encrypted store file is safe to commit. The key file is not.

## Quick troubleshooting

- `esk.yaml not found`: run commands from your project root, or run `esk init`
- `encryption key not found`: run `esk init` to create `.esk/store.key`
- Adapter/plugin CLI errors: install and authenticate required CLIs (for example `wrangler`, `op`, `aws`)
- Unknown environment/app in target: verify names match `environments` and `apps` in `esk.yaml`

## Development

`cargo xtask sandbox` builds a release binary and scaffolds a test project in `/private/tmp/esk-test` with mock CLI shims and sample secrets.

```bash
cargo xtask sandbox
cargo xtask sandbox --clean
```

## License

MIT
