use anyhow::{bail, Context, Result};
use esk::store::SecretStore;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SANDBOX_DIR: &str = "/private/tmp/esk-test";

const CONFIG_YAML: &str = r#"project: demo
environments: [dev, staging, prod]

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
      staging: ".staging"
      prod: ".production"
  cloudflare:
    env_flags:
      prod: "--env production"
  convex:
    path: apps/api
    deployment_source: apps/api/.env.local
    env_flags:
      prod: "--prod"
  fly:
    app_names:
      web: demo-web
      api: demo-api

plugins:
  onepassword:
    vault: Engineering
    item_pattern: "{project} - {Environment}"

secrets:
  Auth:
    AUTH_SECRET:
      description: NextAuth secret key
      targets:
        env: [web:dev, web:staging, web:prod]
        cloudflare: [web:prod]
    SESSION_KEY:
      targets:
        env: [web:dev, web:prod]
  Stripe:
    STRIPE_KEY:
      description: Stripe API key
      targets:
        env: [web:dev, web:prod]
        cloudflare: [web:prod]
        fly: [web:prod]
    STRIPE_WEBHOOK:
      targets:
        env: [web:dev, web:prod]
  Convex:
    CONVEX_URL:
      targets:
        env: [web:dev, web:prod]
        convex: [dev, prod]
  Database:
    DATABASE_URL:
      description: PostgreSQL connection string
      targets:
        env: [api:dev, api:staging, api:prod]
        fly: [api:prod]
"#;

const WRANGLER_SHIM: &str = r#"#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then echo "3.0.0"; exit 0; fi
echo "[mock wrangler] $*"
if [[ "${1:-}" == "secret" && "${2:-}" == "put" ]]; then
  value=$(cat)
  echo "[mock wrangler] would set ${3} = ${value:0:8}..."
fi
"#;

const NPX_SHIM: &str = r#"#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then echo "10.0.0"; exit 0; fi
echo "[mock npx] $*"
if [[ "${1:-}" == "convex" && "${2:-}" == "env" && "${3:-}" == "list" ]]; then
  echo "CONVEX_URL=https://example.convex.cloud"
  exit 0
fi
if [[ "${1:-}" == "convex" && "${2:-}" == "env" && "${3:-}" == "set" ]]; then
  echo "[mock convex] would set ${4} = ${5:0:8}..."
fi
"#;

const OP_SHIM: &str = r#"#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then echo "2.24.0"; exit 0; fi
# For "op vault get" - pretend vault exists
if [[ "${1:-}" == "vault" && "${2:-}" == "get" ]]; then
  echo '{"id":"abc123","name":"Engineering"}'
  exit 0
fi
# For "op item get" - return a simulated remote item (older version than local)
if [[ "${1:-}" == "item" && "${2:-}" == "get" ]]; then
  cat <<'EOF'
{"fields":[{"section":{"label":"Auth"},"label":"AUTH_SECRET","value":"remote-auth-secret"},{"section":{"label":"Stripe"},"label":"STRIPE_KEY","value":"sk_test_remote"},{"section":{"label":"Stripe"},"label":"STRIPE_WEBHOOK","value":"whsec_remote"},{"section":{"label":"_Metadata"},"label":"version","value":"1"}]}
EOF
  exit 0
fi
# For "op item create/edit" - succeed silently
if [[ "${1:-}" == "item" ]]; then
  echo "[mock op] $2 ok" >&2
  exit 0
fi
echo "[mock op] unhandled: $*" >&2
"#;

const FLY_SHIM: &str = r#"#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then echo "0.3.0"; exit 0; fi
if [[ "${1:-}" == "auth" && "${2:-}" == "whoami" ]]; then
  echo "user@example.com"
  exit 0
fi
if [[ "${1:-}" == "secrets" && "${2:-}" == "import" ]]; then
  content=$(cat)
  key=$(echo "$content" | cut -d= -f1)
  echo "[mock fly] would import ${key}=... to app ${4}"
  exit 0
fi
if [[ "${1:-}" == "secrets" && "${2:-}" == "unset" ]]; then
  echo "[mock fly] would unset ${3} from app ${5}"
  exit 0
fi
echo "[mock fly] $*"
"#;

const SECRETS: &[(&str, &str, &str)] = &[
    ("AUTH_SECRET", "dev", "dev-auth-secret-abc123"),
    ("AUTH_SECRET", "staging", "staging-auth-secret-xyz"),
    ("AUTH_SECRET", "prod", "prod-auth-secret-REAL"),
    ("SESSION_KEY", "dev", "dev-session-key-111"),
    ("SESSION_KEY", "prod", "prod-session-key-222"),
    ("STRIPE_KEY", "dev", "sk_test_abc123"),
    ("STRIPE_KEY", "prod", "sk_live_xyz789"),
    ("STRIPE_WEBHOOK", "dev", "whsec_test_abc"),
    ("STRIPE_WEBHOOK", "prod", "whsec_live_xyz"),
    ("CONVEX_URL", "dev", "https://happy-dog-123.convex.cloud"),
    ("CONVEX_URL", "prod", "https://cool-cat-456.convex.cloud"),
    (
        "DATABASE_URL",
        "dev",
        "postgresql://localhost:5432/demo_dev",
    ),
    (
        "DATABASE_URL",
        "staging",
        "postgresql://staging-db:5432/demo",
    ),
    ("DATABASE_URL", "prod", "postgresql://prod-db:5432/demo"),
];

struct Flags {
    skip_build: bool,
    reset: bool,
    clean: bool,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Self> {
        let mut flags = Flags {
            skip_build: false,
            reset: false,
            clean: false,
        };
        for arg in args {
            match arg.as_str() {
                "--skip-build" => flags.skip_build = true,
                "--reset" => flags.reset = true,
                "--clean" => flags.clean = true,
                other => bail!("unknown flag: {other}"),
            }
        }
        Ok(flags)
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("sandbox") => {
            let flags = Flags::parse(&args[1..])?;
            let root = Path::new(SANDBOX_DIR);

            if flags.clean {
                return clean(root);
            }

            if flags.reset {
                // Keep the sandbox dir — just wipe the store so secrets reset to seeded state
                let esk_dir = root.join(".esk");
                if esk_dir.exists() {
                    fs::remove_dir_all(&esk_dir)
                        .with_context(|| format!("failed to remove {}", esk_dir.display()))?;
                }
            } else {
                if !flags.skip_build {
                    build_release()?;
                }
                if root.exists() {
                    fs::remove_dir_all(root)
                        .with_context(|| format!("failed to remove {}", root.display()))?;
                }
            }

            setup(root, &workspace_root())?;
            print_instructions(root);
            Ok(())
        }
        Some(cmd) => bail!("unknown command: {cmd}"),
        None => bail!("usage: cargo xtask sandbox [--skip-build] [--reset] [--clean]"),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate must be inside workspace")
        .to_path_buf()
}

fn build_release() -> Result<()> {
    eprintln!("Building esk...");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "esk"])
        .status()
        .context("failed to run cargo build")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    Ok(())
}

pub fn setup(root: &Path, workspace: &Path) -> Result<()> {
    let bin_dir = root.join(".bin");

    // Create directory structure
    fs::create_dir_all(root.join("apps/web")).context("failed to create apps/web")?;
    fs::create_dir_all(root.join("apps/api")).context("failed to create apps/api")?;
    fs::create_dir_all(&bin_dir).context("failed to create .bin")?;

    // Write config
    fs::write(root.join("esk.yaml"), CONFIG_YAML).context("failed to write esk.yaml")?;

    // Write convex deployment source
    fs::write(
        root.join("apps/api/.env.local"),
        "CONVEX_DEPLOYMENT=dev:happy-dog-123\n",
    )
    .context("failed to write .env.local")?;

    // Write mock shims
    write_executable(&bin_dir.join("wrangler"), WRANGLER_SHIM)?;
    write_executable(&bin_dir.join("npx"), NPX_SHIM)?;
    write_executable(&bin_dir.join("op"), OP_SHIM)?;
    write_executable(&bin_dir.join("fly"), FLY_SHIM)?;

    // Symlink release binary
    let binary = workspace.join("target/release/esk");
    let link = bin_dir.join("esk");
    if link.exists() || link.symlink_metadata().is_ok() {
        fs::remove_file(&link).ok();
    }
    std::os::unix::fs::symlink(&binary, &link).with_context(|| {
        format!(
            "failed to symlink {} -> {}",
            link.display(),
            binary.display()
        )
    })?;

    // Initialize store and seed secrets
    let mut env_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, env, _) in SECRETS {
        *env_counts.entry(env).or_default() += 1;
    }
    let summary = env_counts
        .iter()
        .map(|(env, count)| format!("{env}: {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("Seeding {} secrets ({})...", SECRETS.len(), summary);

    let store = SecretStore::load_or_create(root).context("failed to create store")?;
    for (key, env, value) in SECRETS {
        store.set(key, env, value)?;
    }

    Ok(())
}

fn clean(root: &Path) -> Result<()> {
    if root.exists() {
        eprintln!("Cleaning up {}", root.display());
        fs::remove_dir_all(root).with_context(|| format!("failed to remove {}", root.display()))?;
    } else {
        eprintln!("Nothing to clean ({})", root.display());
    }
    Ok(())
}

fn write_executable(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    Ok(())
}

fn print_instructions(root: &Path) {
    eprintln!();
    eprintln!("=== Test environment ready ===");
    eprintln!();
    eprintln!("  cd {} && export PATH=\"$PWD/.bin:$PATH\"", root.display());
    eprintln!();
    eprintln!("Try:");
    eprintln!("  esk list");
    eprintln!("  esk list --env dev");
    eprintln!("  esk get STRIPE_KEY --env dev");
    eprintln!("  esk status");
    eprintln!("  esk status --env dev");
    eprintln!("  esk status --env prod");
    eprintln!("  esk sync --env dev");
    eprintln!("  esk sync --env dev --dry-run --verbose");
    eprintln!("  esk sync --env prod                    # cloudflare + convex + fly shims");
    eprintln!("  esk set NEW_SECRET --env dev --value test123");
    eprintln!("  esk delete SESSION_KEY --env dev       # then esk list to verify");
    eprintln!("  esk push --env dev                     # push to 1password shim");
    eprintln!("  esk pull --env dev                     # reconcile with remote (local wins)");
    eprintln!();
    eprintln!("Re-seed without rebuilding:");
    eprintln!("  cargo xtask sandbox --reset");
    eprintln!();
}
