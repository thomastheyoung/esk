#!/usr/bin/env bash
set -euo pipefail

REPO="thomastheyoung/lockbox"
INSTALL_DIR="${LOCKBOX_INSTALL_DIR:-$HOME/.local/bin}"

# Detect OS
case "$(uname -s)" in
  Linux*)  os="unknown-linux-gnu" ;;
  Darwin*) os="apple-darwin" ;;
  *)       echo "Error: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

# Detect architecture
case "$(uname -m)" in
  x86_64|amd64)  arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)             echo "Error: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

target="${arch}-${os}"

# Determine version
if [ -n "${LOCKBOX_VERSION:-}" ]; then
  tag="v${LOCKBOX_VERSION#v}"
else
  tag=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
  if [ -z "$tag" ]; then
    echo "Error: could not determine latest release" >&2
    exit 1
  fi
fi

url="https://github.com/${REPO}/releases/download/${tag}/lockbox-${target}.tar.gz"

echo "Installing lockbox ${tag} for ${target}..."

# Download and extract
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

curl -fsSL "$url" | tar xz -C "$tmpdir"

# Install
mkdir -p "$INSTALL_DIR"
mv "$tmpdir/lockbox-${target}/lockbox" "$INSTALL_DIR/lockbox"
chmod +x "$INSTALL_DIR/lockbox"

echo "Installed lockbox to ${INSTALL_DIR}/lockbox"

# Check PATH
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Warning: ${INSTALL_DIR} is not on your \$PATH" >&2 ;;
esac
