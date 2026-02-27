# Targets

Targets deploy secrets to their configured services via `esk deploy`. Each target is configured in the `targets` section of `esk.yaml`. Secrets declare which targets they deploy to — only targeted secrets are deployed.

For sync remotes (1Password, cloud files), see [REMOTES.md](REMOTES.md).

## Overview

| Target                                    | Config key   | External CLI | Deploy mode | Requires app?             |
| ----------------------------------------- | ------------ | ------------ | ----------- | ------------------------- |
| [Env file](#env-file)                     | `env`        | None         | Batch       | Yes                       |
| [Cloudflare Workers](#cloudflare-workers) | `cloudflare` | `wrangler`   | Individual  | Yes (Workers); No (Pages) |
| [Convex](#convex)                         | `convex`     | `npx`        | Individual  | No                        |
| [Fly.io](#flyio)                          | `fly`        | `fly`        | Individual  | Yes                       |
| [Netlify](#netlify)                       | `netlify`    | `netlify`    | Individual  | No                        |
| [Vercel](#vercel)                         | `vercel`     | `vercel`     | Individual  | No                        |
| [GitHub Actions](#github-actions)         | `github`     | `gh`         | Individual  | No                        |
| [Heroku](#heroku)                         | `heroku`     | `heroku`     | Individual  | Yes                       |
| [Supabase](#supabase)                     | `supabase`   | `supabase`   | Individual  | No                        |
| [Railway](#railway)                       | `railway`    | `railway`    | Individual  | No                        |
| [GitLab CI](#gitlab-ci)                   | `gitlab`     | `glab`       | Individual  | No                        |
| [AWS SSM](#aws-ssm)                       | `aws_ssm`    | `aws`        | Individual  | No                        |
| [Kubernetes](#kubernetes)                 | `kubernetes` | `kubectl`    | Batch       | No                        |
| [Docker Swarm](#docker-swarm)             | `docker`     | `docker`     | Individual  | No                        |
| [Custom](#custom)                         | User-defined | User-defined | Individual  | No                        |

**Deploy modes:**

- **Batch** — When any secret changes for a target group, the entire output is regenerated. Used by env file and Kubernetes targets.
- **Individual** — Each changed secret is deployed independently. Used by all other targets.

---

## Env file

Generates `.env` files from the encrypted store. The output path is computed from a configurable pattern using the app path and environment.

### How it works

1. When any secret changes for an (app, environment) pair, the **entire** `.env` file for that pair is regenerated atomically (temp file + rename).
2. Secrets are grouped by vendor with `# === Vendor ===` section headers and sorted alphabetically within each group.
3. The file includes a header comment with instructions for updating and regenerating.
4. Parent directories are created automatically if they don't exist.
5. Generated files are marked read-only to discourage manual edits (for example `0400` on Unix).
6. Multiline values are rejected for `.env` output safety.

### Configuration

```yaml
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
      staging: ".staging"
      prod: ".production"
```

| Field        | Required | Description                                                                                |
| ------------ | -------- | ------------------------------------------------------------------------------------------ |
| `pattern`    | Yes      | Path template for generated files. Supports `{app_path}` and `{env_suffix}` placeholders.  |
| `env_suffix` | No       | Map of environment name to suffix string. Environments not listed default to empty string. |

### Path resolution

The pattern is resolved by replacing:

- `{app_path}` with the app's `path` from the `apps` section
- `{env_suffix}` with the value from `env_suffix` for the current environment (or empty string if not mapped)

The result is relative to the project root (where `esk.yaml` lives).
Resolved paths must stay within the project root; traversal/symlink escape paths are rejected.

**Examples with `pattern: "{app_path}/.env{env_suffix}.local"`:**

| App path   | Environment | Suffix          | Resolved path                    |
| ---------- | ----------- | --------------- | -------------------------------- |
| `apps/web` | `dev`       | `""`            | `apps/web/.env.local`            |
| `apps/web` | `prod`      | `".production"` | `apps/web/.env.production.local` |
| `apps/api` | `staging`   | `".staging"`    | `apps/api/.env.staging.local`    |

### Target format

Targets must include an app: `app:environment`.

```yaml
secrets:
  Stripe:
    STRIPE_KEY:
      targets:
        env: [web:dev, web:prod, api:dev]
```

### Generated output

```bash
# Auto-generated by esk — do not edit manually
#
# Update secrets:  esk set <KEY> --env <ENV>
# Regenerate file: esk deploy --env <ENV>

# === Convex ===
CONVEX_URL=https://example.convex.cloud

# === Stripe ===
STRIPE_KEY=sk_test_abc123
STRIPE_WEBHOOK_SECRET=whsec_xyz
```

---

## Cloudflare Workers

Deploys secrets to Cloudflare Workers using `wrangler secret put`.

### How it works

1. For each secret, runs `wrangler secret put <KEY>` with the value piped via stdin.
2. The command runs in the app's directory (so it picks up the local `wrangler.toml`).
3. Per-environment flags (e.g., `--env production`) are appended to the command.

### Prerequisites

- [Wrangler CLI](https://developers.cloudflare.com/workers/wrangler/) installed and authenticated.
- A `wrangler.toml` in each app directory that uses this target.

### Configuration

```yaml
targets:
  cloudflare:
    mode: workers # "workers" (default) or "pages"
    pages_project: my-pages # required when mode is "pages"
    env_flags:
      dev: ""
      prod: "--env production"
```

| Field           | Required    | Default   | Description                                                                                                                                                                                  |
| --------------- | ----------- | --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `mode`          | No          | `workers` | Secrets API to use: `workers` (wrangler secret) or `pages` (wrangler pages secret).                                                                                                          |
| `pages_project` | Conditional | —         | Cloudflare Pages project name. Required when `mode` is `pages`.                                                                                                                              |
| `env_flags`     | No          | —         | Map of environment name to extra CLI flags passed to Cloudflare secret commands (`put`/`delete` in workers or pages mode). Flags are split on whitespace and appended as separate arguments. |

### Command executed

```bash
# Workers mode (default) — in the app's directory:
echo "<value>" | wrangler secret put <KEY> [env_flags...]
wrangler secret delete <KEY> --force [env_flags...]

# Pages mode:
echo "<value>" | wrangler pages secret put <KEY> --project <pages_project> [env_flags...]
wrangler pages secret delete <KEY> --project <pages_project> --force [env_flags...]
```

For a secret `API_KEY` targeting `web:prod` with `env_flags.prod: "--env production"`:

```bash
cd apps/web && echo "sk_live_..." | wrangler secret put API_KEY --env production
```

### Target format

**Workers mode**: targets must include an app: `app:environment`.

**Pages mode**: targets are environment-only (no app prefix needed).

```yaml
secrets:
  Stripe:
    STRIPE_KEY:
      targets:
        cloudflare: [web:prod]
```

---

## Convex

Deploys environment variables to Convex deployments using `npx convex env set`.

### How it works

1. For each secret, runs `npx convex env set <KEY> <VALUE>` in the configured Convex project directory.
2. If `deployment_source` is set, reads `CONVEX_DEPLOYMENT` from that file and passes it as an environment variable — this tells the Convex CLI which deployment to target.
3. Per-environment flags (e.g., `--prod`) are appended to the command.

### Prerequisites

- Node.js and `npx` available on PATH.
- Convex project initialized in the configured path.
- Authenticated with Convex (e.g., via `npx convex login`).

### Configuration

```yaml
targets:
  convex:
    path: apps/api
    deployment_source: apps/api/.env.local
    env_flags:
      dev: ""
      prod: "--prod"
```

| Field               | Required | Description                                                                                                                                              |
| ------------------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `path`              | Yes      | Path to the Convex project directory (relative to project root). Commands run in this directory.                                                         |
| `deployment_source` | No       | Path to a file containing a `CONVEX_DEPLOYMENT=<value>` line. The value is passed as an env var to the Convex CLI. Quotes around the value are stripped. |
| `env_flags`         | No       | Map of environment name to extra CLI flags. Flags are split on whitespace and appended as separate arguments.                                            |

### Deployment source

The `deployment_source` file is parsed line-by-line looking for `CONVEX_DEPLOYMENT=<value>`. This is typically the `.env.local` file generated by `npx convex dev`, which contains the deployment URL. All of the following formats are supported:

```bash
CONVEX_DEPLOYMENT=dev:my-app-123
CONVEX_DEPLOYMENT="dev:my-app-123"
CONVEX_DEPLOYMENT='dev:my-app-123'
```

If the file doesn't exist or doesn't contain the variable, the command runs without the env var (Convex CLI will use its own default resolution).

### Command executed

```bash
# In the convex path, with CONVEX_DEPLOYMENT set:
CONVEX_DEPLOYMENT=dev:my-app-123 npx convex env set <KEY> <VALUE> [env_flags...]

# Delete:
CONVEX_DEPLOYMENT=dev:my-app-123 npx convex env unset <KEY> [env_flags...]
```

> **Security note**: `npx convex env set` has no stdin support. Secret values are passed as CLI arguments and are visible in process listings (`ps aux`). This is a known limitation of the Convex CLI. A warning is printed at deploy time.

### Target format

Targets are environment-only (no app prefix needed since the target has its own `path`):

```yaml
secrets:
  Auth:
    AUTH_SECRET:
      targets:
        convex: [dev, prod]
```

---

## Fly.io

Deploys secrets to Fly.io apps using `fly secrets import`. Values are piped via stdin to avoid exposing them in process listings.

Values containing newlines are rejected (the target sends `KEY=VALUE` over stdin, and newlines would inject additional variables).

### Prerequisites

- [Fly CLI](https://fly.io/docs/hands-on/install-flyctl/) installed and authenticated (`fly auth login`).

### Configuration

```yaml
targets:
  fly:
    app_names:
      web: my-fly-app
      api: my-fly-api
    env_flags:
      prod: "--stage"
```

| Field       | Required | Description                                                         |
| ----------- | -------- | ------------------------------------------------------------------- |
| `app_names` | Yes      | Maps esk app names to Fly app names.                                |
| `env_flags` | No       | Map of environment name to extra CLI flags appended to the command. |

### Command executed

```bash
# Value piped via stdin as KEY=VALUE:
echo "KEY=VALUE" | fly secrets import -a <fly-app> [env_flags...]

# Delete:
fly secrets unset KEY -a <fly-app> [env_flags...]
```

### Target format

Targets must include an app: `app:environment`.

```yaml
secrets:
  General:
    API_KEY:
      targets:
        fly: [web:dev, web:prod]
```

---

## Netlify

Deploys environment variables to Netlify sites using `netlify env:set`.

> **Security note**: Secret values are passed as CLI arguments and are visible in process listings (`ps aux`). A warning is printed at deploy time.

### Prerequisites

- [Netlify CLI](https://docs.netlify.com/cli/get-started/) installed (`npm install -g netlify-cli`).
- Site linked (`netlify link`) so `netlify status` succeeds during preflight.

Preflight runs `netlify status` to verify CLI installation and site linkage.

### Configuration

```yaml
targets:
  netlify:
    site: my-site-id # optional
    env_flags:
      prod: "--context production"
```

| Field       | Required | Description                                                         |
| ----------- | -------- | ------------------------------------------------------------------- |
| `site`      | No       | Netlify site ID or name. Passed as `--site` flag if set.            |
| `env_flags` | No       | Map of environment name to extra CLI flags appended to the command. |

### Command executed

```bash
netlify env:set KEY VALUE [--site <site>] [env_flags...]
netlify env:unset KEY [--site <site>] [env_flags...]
```

### Target format

Targets are environment-only (no app prefix needed):

```yaml
secrets:
  General:
    API_KEY:
      targets:
        netlify: [dev, prod]
```

---

## Vercel

Deploys environment variables to Vercel projects using `vercel env add` with the value piped via stdin.

### Prerequisites

- [Vercel CLI](https://vercel.com/docs/cli) installed (`npm install -g vercel`) and authenticated (`vercel login`).

### Configuration

```yaml
targets:
  vercel:
    env_names:
      dev: development
      prod: production
    env_flags:
      prod: "--scope my-team"
```

| Field       | Required | Description                                                         |
| ----------- | -------- | ------------------------------------------------------------------- |
| `env_names` | Yes      | Maps esk environment names to Vercel environment names.             |
| `env_flags` | No       | Map of environment name to extra CLI flags appended to the command. |

### Command executed

```bash
echo "<value>" | vercel env add KEY <vercel-env> --force [env_flags...]
vercel env rm KEY <vercel-env> --yes [env_flags...]
```

### Target format

Targets are environment-only:

```yaml
secrets:
  General:
    API_KEY:
      targets:
        vercel: [dev, prod]
```

---

## GitHub Actions

Deploys repository secrets using `gh secret set` with the value piped via stdin to avoid exposing secrets in process listings.

### Prerequisites

- [GitHub CLI](https://cli.github.com/) installed and authenticated (`gh auth login`).

### Configuration

```yaml
targets:
  github:
    repo: owner/repo # optional — defaults to current repo
    env_flags:
      prod: "--env production"
```

| Field       | Required | Description                                                         |
| ----------- | -------- | ------------------------------------------------------------------- |
| `repo`      | No       | GitHub repo in `owner/repo` format. Passed as `-R` flag if set.     |
| `env_flags` | No       | Map of environment name to extra CLI flags appended to the command. |

### Command executed

```bash
echo "<value>" | gh secret set KEY [-R <repo>] [env_flags...]
gh secret delete KEY [-R <repo>] [env_flags...]
```

### Target format

Targets are environment-only:

```yaml
secrets:
  General:
    API_KEY:
      targets:
        github: [dev, prod]
```

---

## Heroku

Deploys config vars to Heroku apps using `heroku config:set`.

> **Security note**: Secret values are passed as CLI arguments and are visible in process listings (`ps aux`). A warning is printed at deploy time.

### Prerequisites

- [Heroku CLI](https://devcenter.heroku.com/articles/heroku-cli) installed and authenticated (`heroku login`).

### Configuration

```yaml
targets:
  heroku:
    app_names:
      web: my-heroku-app
    env_flags:
      prod: "--remote staging"
```

| Field       | Required | Description                                                         |
| ----------- | -------- | ------------------------------------------------------------------- |
| `app_names` | Yes      | Maps esk app names to Heroku app names.                             |
| `env_flags` | No       | Map of environment name to extra CLI flags appended to the command. |

### Command executed

```bash
heroku config:set KEY=VALUE -a <heroku-app> [env_flags...]
heroku config:unset KEY -a <heroku-app> [env_flags...]
```

### Target format

Targets must include an app: `app:environment`.

```yaml
secrets:
  General:
    API_KEY:
      targets:
        heroku: [web:dev, web:prod]
```

---

## Supabase

Deploys secrets to Supabase edge functions using `supabase secrets set`. Values are piped via stdin to avoid exposing them in process listings.

Values containing newlines are rejected (the target sends `KEY=VALUE` over stdin, and newlines would inject additional variables).

### Prerequisites

- [Supabase CLI](https://supabase.com/docs/guides/cli) installed.

### Configuration

```yaml
targets:
  supabase:
    project_ref: abcdef123456
    env_flags:
      prod: "--experimental"
```

| Field         | Required | Description                                                         |
| ------------- | -------- | ------------------------------------------------------------------- |
| `project_ref` | Yes      | Supabase project reference ID.                                      |
| `env_flags`   | No       | Map of environment name to extra CLI flags appended to the command. |

### Command executed

```bash
# Value piped via stdin as KEY=VALUE:
echo "KEY=VALUE" | supabase secrets set --project-ref <ref> [env_flags...]

# Delete:
supabase secrets unset KEY --project-ref <ref> [env_flags...]
```

### Target format

Targets are environment-only:

```yaml
secrets:
  General:
    API_KEY:
      targets:
        supabase: [dev, prod]
```

---

## Railway

Deploys environment variables to Railway projects using `railway variables --set`.

> **Security note**: Secret values are passed as CLI arguments and are visible in process listings (`ps aux`). A warning is printed at deploy time.

### Prerequisites

- [Railway CLI](https://docs.railway.app/guides/cli) installed and authenticated (`railway login`).

### Configuration

```yaml
targets:
  railway:
    env_flags:
      prod: "--environment production"
```

| Field       | Required | Description                                                         |
| ----------- | -------- | ------------------------------------------------------------------- |
| `env_flags` | No       | Map of environment name to extra CLI flags appended to the command. |

### Command executed

```bash
railway variables --set "KEY=VALUE" [env_flags...]
railway variables delete KEY [env_flags...]
```

### Target format

Targets are environment-only:

```yaml
secrets:
  General:
    API_KEY:
      targets:
        railway: [dev, prod]
```

---

## GitLab CI

Deploys CI/CD variables to GitLab projects using `glab variable set`.

### Prerequisites

- [GitLab CLI](https://gitlab.com/gitlab-org/cli) installed and authenticated (`glab auth login`).

### Configuration

```yaml
targets:
  gitlab:
    env_flags:
      prod: "--masked"
```

| Field       | Required | Description                                                         |
| ----------- | -------- | ------------------------------------------------------------------- |
| `env_flags` | No       | Map of environment name to extra CLI flags appended to the command. |

### Command executed

```bash
# Value piped via stdin (not a positional argument):
echo -n "<value>" | glab variable set KEY --scope <env> [env_flags...]

# Delete:
glab variable delete KEY --scope <env> [env_flags...]
```

### Target format

Targets are environment-only (the environment name is used as the `--scope` value):

```yaml
secrets:
  General:
    API_KEY:
      targets:
        gitlab: [dev, prod]
```

---

## AWS SSM

Deploys secrets to AWS Systems Manager Parameter Store using `aws ssm put-parameter`. Values are sent via stdin (`--cli-input-json file:///dev/stdin`) to avoid exposing secrets in process listings.

### Prerequisites

- [AWS CLI](https://aws.amazon.com/cli/) installed and authenticated (`aws configure`).

Preflight runs `aws sts get-caller-identity` to verify credentials and connectivity.

### Configuration

```yaml
targets:
  aws_ssm:
    path_prefix: "/{project}/{environment}/"
    region: us-east-1
    profile: staging
    parameter_type: SecureString
    env_flags:
      prod: "--no-paginate"
```

| Field            | Required | Default        | Description                                                                               |
| ---------------- | -------- | -------------- | ----------------------------------------------------------------------------------------- |
| `path_prefix`    | Yes      | —              | Path prefix with `{project}` and `{environment}` interpolation. The key name is appended. |
| `region`         | No       | —              | AWS region. Passed as `--region` flag.                                                    |
| `profile`        | No       | —              | AWS profile. Passed as `--profile` flag.                                                  |
| `parameter_type` | No       | `SecureString` | SSM parameter type: `SecureString`, `String`, or `StringList`.                            |
| `env_flags`      | No       | —              | Map of environment name to extra CLI flags appended to the command.                       |

### Path resolution

The parameter name is built by replacing `{project}` and `{environment}` in `path_prefix`, then appending the key name.

**Example with `path_prefix: "/{project}/{environment}/"`:**

| Project | Environment | Key       | Parameter name        |
| ------- | ----------- | --------- | --------------------- |
| `myapp` | `dev`       | `DB_PASS` | `/myapp/dev/DB_PASS`  |
| `myapp` | `prod`      | `API_KEY` | `/myapp/prod/API_KEY` |

### Command executed

```bash
# Put parameter (value via stdin as JSON):
echo '{"Name":"/myapp/dev/KEY","Value":"...","Type":"SecureString","Overwrite":true}' | \
  aws ssm put-parameter --cli-input-json file:///dev/stdin [--region ...] [--profile ...] [env_flags...]

# Delete parameter:
aws ssm delete-parameter --name /myapp/dev/KEY [--region ...] [--profile ...] [env_flags...]
```

### Target format

Targets are environment-only:

```yaml
secrets:
  General:
    API_KEY:
      targets:
        aws_ssm: [dev, prod]
```

---

## Kubernetes

Generates Kubernetes Secret manifests and applies them using `kubectl apply`. This is a **batch** target — when any secret changes, the entire Secret resource is regenerated and applied.

### How it works

1. Collects all secrets for the (environment) target group.
2. Generates a YAML Secret manifest with base64-encoded values.
3. Pipes the manifest to `kubectl apply -f -` via stdin.

### Prerequisites

- [kubectl](https://kubernetes.io/docs/tasks/tools/) installed and configured with access to the target cluster(s).

Preflight runs `kubectl cluster-info` to verify cluster connectivity.

### Configuration

```yaml
targets:
  kubernetes:
    namespace:
      dev: myapp-dev
      prod: myapp-prod
    secret_name: my-app-secrets
    context:
      prod: prod-cluster
    env_flags:
      prod: "--dry-run=client"
```

| Field         | Required | Default             | Description                                                         |
| ------------- | -------- | ------------------- | ------------------------------------------------------------------- |
| `namespace`   | Yes      | —                   | Maps esk environment names to Kubernetes namespaces.                |
| `secret_name` | No       | `{project}-secrets` | Name of the Kubernetes Secret resource.                             |
| `context`     | No       | —                   | Maps esk environment names to kubectl contexts (`--context` flag).  |
| `env_flags`   | No       | —                   | Map of environment name to extra CLI flags appended to the command. |

### Generated manifest

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: myapp-secrets
  namespace: myapp-dev
type: Opaque
data:
  DB_HOST: bG9jYWxob3N0 # base64("localhost")
  DB_PASS: czNjcmV0 # base64("s3cret")
```

### Command executed

```bash
echo "<manifest>" | kubectl apply -f - [--context <ctx>] [env_flags...]
```

### Target format

Targets are environment-only:

```yaml
secrets:
  General:
    DB_HOST:
      targets:
        kubernetes: [dev, prod]
```

---

## Docker Swarm

Deploys secrets to Docker Swarm using `docker secret create`. Docker Swarm secrets are encrypted at rest in the Raft log and mounted as tmpfs files at `/run/secrets/<name>` inside containers — never exposed as environment variables or CLI arguments.

### How it works

1. Docker secrets are **immutable** — you cannot update a secret in place.
2. On sync, esk removes the existing secret (`docker secret rm`) then recreates it (`docker secret create`). The remove step tolerates "no such secret" errors for first-time creates.
3. Values are piped via stdin (`docker secret create <name> -`) to avoid exposing them in process listings.
4. Services must be restarted to pick up new secret values regardless of the update method.
5. If a secret is currently in use by a service, the remove will fail — this is a Docker constraint the user must resolve (e.g., by updating the service to remove the secret reference first).

### Prerequisites

- [Docker](https://docs.docker.com/get-docker/) installed with the daemon running.
- Swarm mode active (`docker swarm init`).

Preflight runs `docker info --format {{.Swarm.LocalNodeState}}` and verifies the output is `active`.

### Configuration

```yaml
targets:
  docker:
    name_pattern: "{project}-{environment}-{key}" # optional, this is the default
    labels: # optional
      managed-by: esk
    env_flags: # optional
      prod: "--context prod-swarm"
```

| Field          | Required | Default                         | Description                                                                                         |
| -------------- | -------- | ------------------------------- | --------------------------------------------------------------------------------------------------- |
| `name_pattern` | No       | `{project}-{environment}-{key}` | Name template for Docker secrets. Supports `{project}`, `{environment}`, and `{key}` placeholders.  |
| `labels`       | No       | —                               | Static `--label key=value` flags applied to all created secrets. Useful for organizational tagging. |
| `env_flags`    | No       | —                               | Map of environment name to extra CLI flags appended to docker commands.                             |

### Name resolution

Docker secrets are global within a swarm. The `name_pattern` prevents collisions across environments and projects.

**Examples with `name_pattern: "{project}-{environment}-{key}"`:**

| Project | Environment | Key            | Docker secret name       |
| ------- | ----------- | -------------- | ------------------------ |
| `myapp` | `dev`       | `DATABASE_URL` | `myapp-dev-DATABASE_URL` |
| `myapp` | `prod`      | `API_KEY`      | `myapp-prod-API_KEY`     |

### Command executed

```bash
# Remove existing (tolerates "no such secret"):
docker secret rm <name> [env_flags...]

# Create via stdin:
docker secret create [--label key=value ...] <name> - [env_flags...]

# Delete:
docker secret rm <name> [env_flags...]
```

### Target format

Targets are environment-only (Docker secrets are swarm-global, not scoped to an app):

```yaml
secrets:
  General:
    API_KEY:
      targets:
        docker: [dev, prod]
```

---

## Custom

Define your own deploy targets by specifying commands directly in `esk.yaml`. Useful for services that esk doesn't have a built-in target for — internal APIs, niche platforms, or custom scripts.

Custom targets are individual-mode only (one secret at a time).

### How it works

1. On deploy, esk substitutes template variables in the command args and stdin, then executes the program.
2. Non-zero exit codes are treated as deploy failures.
3. If `preflight` is configured, it runs before any deploys to verify the external service is reachable.
4. If `delete` is configured, it's called when pruning orphaned secrets. Otherwise, delete is a no-op.

### Configuration

Custom targets live under `targets.custom` as a named map. Secrets reference them by name, the same way they reference built-in targets.

```yaml
targets:
  custom:
    my-api:
      deploy:
        program: curl
        args: ["-X", "POST", "-d", "@-", "https://api.example.com/secrets/{{key}}"]
        stdin: "{{value}}"
      delete:
        program: curl
        args: ["-X", "DELETE", "https://api.example.com/secrets/{{key}}?env={{env}}"]
      preflight:
        program: curl
        args: ["--fail", "-s", "https://api.example.com/health"]
      env_flags:
        prod: "--header X-Env:production"
```

| Field       | Required | Description                                                                        |
| ----------- | -------- | ---------------------------------------------------------------------------------- |
| `deploy`    | Yes      | Command to run for each secret. Must have `program` and `args`.                    |
| `delete`    | No       | Command to run when pruning orphaned secrets. Same structure as `deploy`.          |
| `preflight` | No       | Command to run before any deploys to check service availability. Same structure.   |
| `env_flags` | No       | Map of environment name to extra CLI flags appended to deploy and delete commands. |

Each command block (`deploy`, `delete`, `preflight`) has:

| Field     | Required | Description                                              |
| --------- | -------- | -------------------------------------------------------- |
| `program` | Yes      | Executable to run (must be in PATH).                     |
| `args`    | Yes      | List of arguments. Supports template variables.          |
| `stdin`   | No       | String piped to the command's stdin. Supports templates. |

### Template variables

Variables are substituted in `args` and `stdin` at deploy time:

| Variable    | Value                                            |
| ----------- | ------------------------------------------------ |
| `{{key}}`   | Secret name (e.g., `API_KEY`)                    |
| `{{value}}` | Secret value                                     |
| `{{env}}`   | Environment name (e.g., `prod`)                  |
| `{{app}}`   | App name, or empty string if the secret has none |

> **Security note**: Prefer `stdin` for `{{value}}` rather than putting it in `args`. Values in args are visible in process listings (`ps aux`). esk warns at deploy time if `{{value}}` appears in deploy args.

### Naming rules

- Names must contain only `a-z`, `A-Z`, `0-9`, `_`, `-`.
- Names cannot collide with built-in target names (`env`, `cloudflare`, `convex`, `fly` etc, see the full list at the top of this doc).

### Target format

Targets are environment-only (no app prefix):

```yaml
secrets:
  General:
    API_KEY:
      targets:
        my-api: [dev, prod]
```

### Examples

**Internal API with token auth:**

```yaml
targets:
  custom:
    internal-api:
      deploy:
        program: curl
        args:
          [
            "-X",
            "PUT",
            "-H",
            "Authorization: Bearer $TOKEN",
            "https://config.internal/secrets/{{key}}?env={{env}}",
          ]
        stdin: "{{value}}"
      delete:
        program: curl
        args:
          [
            "-X",
            "DELETE",
            "-H",
            "Authorization: Bearer $TOKEN",
            "https://config.internal/secrets/{{key}}?env={{env}}",
          ]
      preflight:
        program: curl
        args: ["--fail", "-s", "https://config.internal/health"]
```

**Custom script wrapper:**

```yaml
targets:
  custom:
    my-vault:
      deploy:
        program: ./scripts/deploy-secret.sh
        args: ["{{key}}", "{{env}}"]
        stdin: "{{value}}"
```
