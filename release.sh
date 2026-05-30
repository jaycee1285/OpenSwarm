#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
CARGO_TOML="$REPO_ROOT/Cargo.toml"
REPO="jaycee1285/OpenSwarm"

VERSION=$(grep -oP '^version\s*=\s*"\K[^"]+' "$CARGO_TOML" | head -1)
TAG="v${VERSION}"
APP_NAME="openswarm"
ARCH="$(uname -m)"
PLATFORM="$(uname -s | tr '[:upper:]' '[:lower:]')"
TARBALL="${APP_NAME}-${TAG}-${PLATFORM}-${ARCH}.tar.xz"
ASSET_URL="https://github.com/${REPO}/releases/download/${TAG}/${TARBALL}"
SKIP_UPLOAD="${SKIP_UPLOAD:-0}"

echo "==> Building ${APP_NAME} ${TAG} (${PLATFORM}/${ARCH})"

cd "$REPO_ROOT"
nix build .#default

# `wrapProgram` produces launcher stubs in `bin/openswarm` and
# `bin/.openswarm-wrapped_`. The actual Rust executable is
# `bin/.openswarm-wrapped`, which is what downstream tarball consumers need.
RAW_BINARY="$REPO_ROOT/result/bin/.${APP_NAME}-wrapped"
BINARY="$REPO_ROOT/result/bin/${APP_NAME}"
if [[ -f "$RAW_BINARY" ]]; then
  BINARY="$RAW_BINARY"
fi

if [[ ! -f "$BINARY" ]]; then
  echo "ERROR: Binary not found at ${BINARY} or ${RAW_BINARY}"
  exit 1
fi

STAGING=$(mktemp -d)
trap "rm -rf $STAGING" EXIT

install -m755 "$BINARY" "$STAGING/${APP_NAME}"

install -d "$STAGING/share/applications" "$STAGING/share/icons"
install -m644 "$REPO_ROOT/packaging/linux/openswarm.desktop" \
  "$STAGING/share/applications/openswarm.desktop"
cp -r "$REPO_ROOT/icons/linux/hicolor" "$STAGING/share/icons/"

# Strip Nix store paths for cross-machine portability.
# Building via `nix build` bakes this machine's /nix/store paths into the
# binary's RPATH and ELF interpreter. Those are unique per machine.
# autoPatchelfHook on the receiving machine will set correct paths at install.
echo "==> Stripping Nix store paths for cross-machine portability"
patchelf --remove-rpath "$STAGING/${APP_NAME}"
patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2 "$STAGING/${APP_NAME}"

echo "==> Creating ${TARBALL}"
tar -cJf "$REPO_ROOT/$TARBALL" -C "$STAGING" "${APP_NAME}" share

if [[ "$SKIP_UPLOAD" == "1" ]]; then
  echo "==> SKIP_UPLOAD=1, leaving tarball at $REPO_ROOT/$TARBALL"
  exit 0
fi

echo "==> Uploading to GitHub release ${TAG}"
if gh release view "$TAG" --repo "$REPO" &>/dev/null; then
  gh release upload "$TAG" "$REPO_ROOT/$TARBALL" --repo "$REPO" --clobber
else
  gh release create "$TAG" "$REPO_ROOT/$TARBALL" \
    --repo "$REPO" \
    --title "${APP_NAME} ${TAG}" \
    --notes "${APP_NAME} ${TAG}" \
    --latest
fi

echo "==> Done! https://github.com/${REPO}/releases/tag/${TAG}"

echo "==> Release asset: ${ASSET_URL}"
echo "==> SHA-256 for Nix flake input:"
if [[ "${SKIP_UPLOAD:-0}" == "1" ]]; then
  nix hash file --type sha256 "$REPO_ROOT/$TARBALL"
else
  PREFETCH_JSON=$(nix store prefetch-file --json --hash-type sha256 "$ASSET_URL")
  echo "$PREFETCH_JSON" | grep -oP '"hash"\s*:\s*"\K[^"]+'
  PREFETCH_PATH=$(echo "$PREFETCH_JSON" | grep -oP '"storePath"\s*:\s*"\K[^"]+')
  if [[ -n "$PREFETCH_PATH" ]]; then
    nix store delete "$PREFETCH_PATH" 2>/dev/null || true
  fi
fi
