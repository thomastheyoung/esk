use anyhow::{bail, Context, Result};
use lockbox::store::SecretStore;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SANDBOX_DIR: &str = "/private/tmp/lockbox-test";

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
"#;

const WRANGLER_SHIM: &str = r#"#!/usr/bin/env bash
echo "[mock wrangler] $*"
if [[ "${1:-}" == "secret" && "${2:-}" == "put" ]]; then
  value=$(cat)
  echo "[mock wrangler] would set ${3} = ${value:0:8}..."
fi
"#;

const NPX_SHIM: &str = r#"#!/usr/bin/env bash
echo "[mock npx] $*"
if [[ "${1:-}" == "convex" && "${2:-}" == "env" && "${3:-}" == "set" ]]; then
  echo "[mock convex] would set ${4} = ${5:0:8}..."
fi
"#;

const OP_SHIM: &str = r#"#!/usr/bin/env bash
echo "[mock op] $*"
# For "op item get" - pretend item doesn't exist (triggers create)
if [[ "${1:-}" == "item" && "${2:-}" == "get" ]]; then
  echo "[mock op] item not found" >&2
  exit 1
fi
# For "op item create/edit" - succeed silently
if [[ "${1:-}" == "item" ]]; then
  echo "[mock op] ok"
fi
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("sandbox") => {
            if args.get(1).map(|s| s.as_str()) == Some("--clean") {
                clean(Path::new(SANDBOX_DIR))
            } else {
                build_release()?;
                let root = Path::new(SANDBOX_DIR);
                if root.exists() {
                    fs::remove_dir_all(root)
                        .with_context(|| format!("failed to remove {}", root.display()))?;
                }
                setup(root, &workspace_root())?;
                print_instructions(root);
                Ok(())
            }
        }
        Some(cmd) => bail!("unknown command: {cmd}"),
        None => bail!("usage: cargo xtask sandbox [--clean]"),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate must be inside workspace")
        .to_path_buf()
}

fn build_release() -> Result<()> {
    eprintln!("Building lockbox...");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "lockbox"])
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
    fs::write(root.join("lockbox.yaml"), CONFIG_YAML).context("failed to write lockbox.yaml")?;

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

    // Symlink release binary
    let binary = workspace.join("target/release/lockbox");
    let link = bin_dir.join("lockbox");
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
    eprintln!("Seeding secrets...");
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
    let bin_dir = root.join(".bin");
    eprintln!();
    eprintln!("=== Test environment ready ===");
    eprintln!();
    eprintln!("  cd {}", root.display());
    eprintln!("  export PATH=\"{}:$PATH\"", bin_dir.display());
    eprintln!();
    eprintln!("Try:");
    eprintln!("  lockbox list");
    eprintln!("  lockbox list --env dev");
    eprintln!("  lockbox get STRIPE_KEY --env dev");
    eprintln!("  lockbox status");
    eprintln!("  lockbox status --env prod");
    eprintln!("  lockbox sync --env dev");
    eprintln!("  lockbox sync --env dev --dry-run --verbose");
    eprintln!("  lockbox set NEW_SECRET --env dev --value test123");
    eprintln!("  lockbox push --env dev");
    eprintln!();
}
