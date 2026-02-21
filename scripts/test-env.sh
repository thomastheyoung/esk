#!/usr/bin/env bash
# Sets up a manual test environment for lockbox in /private/tmp/lockbox-test.
# Builds a release binary, scaffolds a project with mock CLI shims,
# and seeds it with sample secrets.
#
# Usage:
#   ./scripts/test-env.sh          # fresh setup
#   ./scripts/test-env.sh --clean  # tear down

set -euo pipefail

TEST_DIR="/private/tmp/lockbox-test"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BIN_DIR="$TEST_DIR/.bin"

# --- Clean mode ---
if [[ "${1:-}" == "--clean" ]]; then
  echo "Cleaning up $TEST_DIR"
  rm -rf "$TEST_DIR"
  exit 0
fi

# --- Build release binary ---
echo "Building lockbox..."
cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml" 2>&1 | tail -1

# --- Scaffold test project ---
echo "Setting up test project in $TEST_DIR"
rm -rf "$TEST_DIR"
mkdir -p "$TEST_DIR/apps/web" "$TEST_DIR/apps/api" "$BIN_DIR"

# Symlink the binary
ln -sf "$PROJECT_DIR/target/release/lockbox" "$BIN_DIR/lockbox"

# --- Mock CLI shims ---
# These echo what they'd do instead of calling real services.

cat > "$BIN_DIR/wrangler" << 'SHIM'
#!/usr/bin/env bash
echo "[mock wrangler] $*"
if [[ "${1:-}" == "secret" && "${2:-}" == "put" ]]; then
  value=$(cat)
  echo "[mock wrangler] would set ${3} = ${value:0:8}..."
fi
SHIM
chmod +x "$BIN_DIR/wrangler"

cat > "$BIN_DIR/npx" << 'SHIM'
#!/usr/bin/env bash
echo "[mock npx] $*"
if [[ "${1:-}" == "convex" && "${2:-}" == "env" && "${3:-}" == "set" ]]; then
  echo "[mock convex] would set ${4} = ${5:0:8}..."
fi
SHIM
chmod +x "$BIN_DIR/npx"

cat > "$BIN_DIR/op" << 'SHIM'
#!/usr/bin/env bash
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
SHIM
chmod +x "$BIN_DIR/op"

# --- Config file ---
cat > "$TEST_DIR/lockbox.yaml" << 'YAML'
project: demo
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
YAML

# --- Create a convex deployment source (needed for convex adapter) ---
cat > "$TEST_DIR/apps/api/.env.local" << 'ENV'
CONVEX_DEPLOYMENT=dev:happy-dog-123
ENV

# --- Initialize store and seed secrets ---
LB="$BIN_DIR/lockbox"
cd "$TEST_DIR"

# PATH override so lockbox finds our mock CLIs
export PATH="$BIN_DIR:$PATH"

"$LB" init 2>/dev/null || true  # idempotent

echo "Seeding secrets..."
"$LB" set AUTH_SECRET --env dev --value "dev-auth-secret-abc123" --no-sync
"$LB" set AUTH_SECRET --env staging --value "staging-auth-secret-xyz" --no-sync
"$LB" set AUTH_SECRET --env prod --value "prod-auth-secret-REAL" --no-sync
"$LB" set SESSION_KEY --env dev --value "dev-session-key-111" --no-sync
"$LB" set SESSION_KEY --env prod --value "prod-session-key-222" --no-sync
"$LB" set STRIPE_KEY --env dev --value "sk_test_abc123" --no-sync
"$LB" set STRIPE_KEY --env prod --value "sk_live_xyz789" --no-sync
"$LB" set STRIPE_WEBHOOK --env dev --value "whsec_test_abc" --no-sync
"$LB" set STRIPE_WEBHOOK --env prod --value "whsec_live_xyz" --no-sync
"$LB" set CONVEX_URL --env dev --value "https://happy-dog-123.convex.cloud" --no-sync
"$LB" set CONVEX_URL --env prod --value "https://cool-cat-456.convex.cloud" --no-sync
"$LB" set DATABASE_URL --env dev --value "postgresql://localhost:5432/demo_dev" --no-sync
"$LB" set DATABASE_URL --env staging --value "postgresql://staging-db:5432/demo" --no-sync
"$LB" set DATABASE_URL --env prod --value "postgresql://prod-db:5432/demo" --no-sync

echo ""
echo "=== Test environment ready ==="
echo ""
echo "  cd $TEST_DIR"
echo "  export PATH=\"$BIN_DIR:\$PATH\""
echo ""
echo "Try:"
echo "  lockbox list"
echo "  lockbox list --env dev"
echo "  lockbox get STRIPE_KEY dev"
echo "  lockbox status"
echo "  lockbox status --env prod"
echo "  lockbox sync --env dev"
echo "  lockbox sync --env dev --dry-run --verbose"
echo "  lockbox set NEW_SECRET dev --value test123"
echo "  lockbox push --env dev"
echo ""
