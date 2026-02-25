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

targets:
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

remotes:
  1password:
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

struct ReleaseFlags {
    dry_run: bool,
    skip_checks: bool,
    allow_dirty: bool,
}

impl ReleaseFlags {
    fn parse(args: &[String]) -> Result<Self> {
        let mut flags = ReleaseFlags {
            dry_run: false,
            skip_checks: false,
            allow_dirty: false,
        };
        for arg in args {
            match arg.as_str() {
                "--dry-run" => flags.dry_run = true,
                "--skip-checks" => flags.skip_checks = true,
                "--allow-dirty" => flags.allow_dirty = true,
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
        Some("release") => {
            let flags = ReleaseFlags::parse(&args[1..])?;
            release(flags)
        }
        Some(cmd) => bail!("unknown command: {cmd}"),
        None => bail!(
            "usage:\n  cargo xtask sandbox [--skip-build] [--reset] [--clean]\n  cargo xtask release [--dry-run] [--skip-checks] [--allow-dirty]"
        ),
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

fn release(flags: ReleaseFlags) -> Result<()> {
    let root = workspace_root();
    ensure_on_main_branch(&root)?;
    if !flags.allow_dirty {
        ensure_clean_worktree(&root)?;
    }

    let version = cargo_package_version(&root)?;
    let tag = format!("v{version}");
    ensure_local_tag_missing(&root, &tag)?;
    ensure_remote_tag_missing(&root, &tag)?;

    if flags.dry_run {
        eprintln!("Dry run:");
        eprintln!("  version: {version}");
        eprintln!("  tag: {tag}");
        eprintln!(
            "  checks: {}",
            if flags.skip_checks { "skip" } else { "run" }
        );
        eprintln!("  commands:");
        eprintln!("    git pull --rebase origin main");
        if !flags.skip_checks {
            eprintln!("    cargo fmt --check");
            eprintln!("    cargo clippy -- -D warnings");
            eprintln!("    cargo test");
        }
        eprintln!("    git push origin main");
        eprintln!("    git tag -a {tag} -m \"release {tag}\"");
        eprintln!("    git push origin {tag}");
        return Ok(());
    }

    run_cmd(&root, "git", &["pull", "--rebase", "origin", "main"])?;
    if !flags.skip_checks {
        run_cmd(&root, "cargo", &["fmt", "--check"])?;
        run_cmd(&root, "cargo", &["clippy", "--", "-D", "warnings"])?;
        run_cmd(&root, "cargo", &["test"])?;
    }
    run_cmd(&root, "git", &["push", "origin", "main"])?;
    run_cmd(
        &root,
        "git",
        &["tag", "-a", &tag, "-m", &format!("release {tag}")],
    )?;
    run_cmd(&root, "git", &["push", "origin", &tag])?;
    eprintln!("Released {tag}. GitHub Actions should now run the Release workflow.");
    Ok(())
}

fn run_cmd(root: &Path, program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .current_dir(root)
        .args(args)
        .status()
        .with_context(|| format!("failed to run command: {program} {}", args.join(" ")))?;
    if !status.success() {
        bail!(
            "command failed (exit {}): {program} {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            args.join(" ")
        );
    }
    Ok(())
}

fn capture_stdout(root: &Path, program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run command: {program} {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "command failed (exit {}): {program} {}\n{}",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            args.join(" "),
            stderr.trim()
        );
    }
    String::from_utf8(output.stdout)
        .context("command produced non-utf8 output")
        .map(|s| s.trim().to_string())
}

fn ensure_clean_worktree(root: &Path) -> Result<()> {
    let status = capture_stdout(root, "git", &["status", "--porcelain"])?;
    if !status.is_empty() {
        bail!("working tree is dirty; commit or stash changes first (or use --allow-dirty)");
    }
    Ok(())
}

fn ensure_on_main_branch(root: &Path) -> Result<()> {
    let branch = capture_stdout(root, "git", &["branch", "--show-current"])?;
    if branch != "main" {
        bail!("release must run from main branch (current: {branch})");
    }
    Ok(())
}

fn ensure_local_tag_missing(root: &Path, tag: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")])
        .output()
        .context("failed to check local tag")?;
    if output.status.success() {
        bail!("tag already exists locally: {tag}");
    }
    Ok(())
}

fn ensure_remote_tag_missing(root: &Path, tag: &str) -> Result<()> {
    let output = capture_stdout(root, "git", &["ls-remote", "--tags", "origin", tag])?;
    if !output.is_empty() {
        bail!("tag already exists on origin: {tag}");
    }
    Ok(())
}

fn cargo_package_version(root: &Path) -> Result<String> {
    let pkgid = capture_stdout(root, "cargo", &["pkgid", "-p", "esk"])?;
    let (_, version) = pkgid
        .rsplit_once('#')
        .with_context(|| format!("unexpected cargo pkgid output: {pkgid}"))?;
    Ok(version.to_string())
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
    eprintln!("  esk deploy --env dev");
    eprintln!("  esk deploy --env dev --dry-run --verbose");
    eprintln!("  esk deploy --env prod                  # cloudflare + convex + fly shims");
    eprintln!("  esk set NEW_SECRET --env dev --value test123");
    eprintln!("  esk delete SESSION_KEY --env dev       # then esk list to verify");
    eprintln!("  esk sync --env dev                     # sync with 1password shim");
    eprintln!();
    eprintln!("Re-seed without rebuilding:");
    eprintln!("  cargo xtask sandbox --reset");
    eprintln!();
}
