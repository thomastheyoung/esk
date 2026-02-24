#!/usr/bin/env bash
set -euo pipefail

REPO="thomastheyoung/esk"
INSTALL_DIR="${ESK_INSTALL_DIR:-$HOME/.local/bin}"

# Detect OS
case "$(uname -s)" in
  Linux*)
    libc="gnu"
    if command -v ldd >/dev/null 2>&1; then
      if ldd --version 2>&1 | grep -qi "musl"; then
        libc="musl"
      fi
    elif [ -f /etc/alpine-release ]; then
      libc="musl"
    fi
    os="unknown-linux-${libc}"
    ;;
  Darwin*) os="apple-darwin" ;;
  *)       echo "Error: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

# Detect architecture
case "$(uname -m)" in
  x86_64|amd64)  arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)             echo "Error: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [ "$os" = "unknown-linux-musl" ] && [ "$arch" != "x86_64" ]; then
  echo "Error: no prebuilt release for ${arch}-unknown-linux-musl" >&2
  echo "Hint: use cargo install esk or build from source." >&2
  exit 1
fi

target="${arch}-${os}"

# Determine version
if [ -n "${ESK_VERSION:-}" ]; then
  tag="v${ESK_VERSION#v}"
else
  latest_api="https://api.github.com/repos/${REPO}/releases/latest"
  latest_json=$(curl -fsSL "$latest_api" || true)
  tag=$(printf "%s\n" "$latest_json" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
  if [ -z "$tag" ]; then
    echo "Error: could not determine latest release from ${latest_api}" >&2
    echo "Hint: set ESK_VERSION explicitly (for example: ESK_VERSION=0.1.0)." >&2
    exit 1
  fi
fi

url="https://github.com/${REPO}/releases/download/${tag}/esk-${target}.tar.gz"

echo "Installing esk ${tag} for ${target}..."

# Download and extract
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

archive="$tmpdir/esk-${target}.tar.gz"
if ! curl -fsSL "$url" -o "$archive"; then
  echo "Error: failed to download release artifact: ${url}" >&2
  exit 1
fi
tar xzf "$archive" -C "$tmpdir"

# Install
mkdir -p "$INSTALL_DIR"
mv "$tmpdir/esk-${target}/esk" "$INSTALL_DIR/esk"
chmod +x "$INSTALL_DIR/esk"

echo "Installed esk to ${INSTALL_DIR}/esk"

# Check PATH
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Warning: ${INSTALL_DIR} is not on your \$PATH" >&2 ;;
esac
