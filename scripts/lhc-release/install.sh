#!/bin/sh
set -eu

REPO="${CODEX_LHC_REPOSITORY:-liminal-ai/codex-lhc}"
PREFIX="${CODEX_LHC_PREFIX:-${HOME}/.local}"
STORE="${CODEX_LHC_INSTALL_ROOT:-${XDG_DATA_HOME:-${HOME}/.local/share}/codex-lhc}"
VERSION="${CODEX_LHC_VERSION:-}"
NAME="${CODEX_LHC_NAME:-}"
ASSET_DIR="${CODEX_LHC_ASSET_DIR:-}"
UNINSTALL=0

usage() {
  cat <<'EOF'
Install Codex + LHC from a codex-lhc GitHub release.

Usage: install.sh [OPTIONS]

  --version VERSION    Install a specific release (default: latest)
  --name NAME          Command name (default: codex, or codex-lhc if codex exists)
  --prefix DIR         Command prefix (default: ~/.local)
  --install-root DIR   Versioned package storage
  --asset-dir DIR      Install from a validated local candidate directory
  --uninstall          Remove the selected command and managed package store
  -h, --help           Show this help

The installer never edits ~/.codex or an LHC archive. Codex + LHC retains full
session transcripts and derivations, so its storage can be substantially larger
than stock Codex during long-running use.
EOF
}

die() { printf 'codex-lhc installer: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) [ "$#" -ge 2 ] || die "--version requires a value"; VERSION=$2; shift 2 ;;
    --name) [ "$#" -ge 2 ] || die "--name requires a value"; NAME=$2; shift 2 ;;
    --prefix) [ "$#" -ge 2 ] || die "--prefix requires a value"; PREFIX=$2; shift 2 ;;
    --install-root) [ "$#" -ge 2 ] || die "--install-root requires a value"; STORE=$2; shift 2 ;;
    --asset-dir) [ "$#" -ge 2 ] || die "--asset-dir requires a value"; ASSET_DIR=$2; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

case "${NAME}" in
  */*) die "--name must be a command name, not a path" ;;
esac
case "$STORE" in
  ''|/|"$HOME") die "refusing unsafe install root: $STORE" ;;
esac

BIN_DIR="${PREFIX}/bin"
if [ -z "$NAME" ]; then
  if [ -f "$STORE/installed-name" ]; then
    NAME=$(cat "$STORE/installed-name")
  elif [ -e "${BIN_DIR}/codex" ] || command -v codex >/dev/null 2>&1; then
    NAME=codex-lhc
  else
    NAME=codex
  fi
fi
LINK="${BIN_DIR}/${NAME}"

if [ "$UNINSTALL" -eq 1 ]; then
  if [ -L "$LINK" ]; then
    target=$(readlink "$LINK")
    case "$target" in
      "$STORE"/*) rm -f "$LINK" ;;
      *) die "$LINK is not managed by this installer" ;;
    esac
  elif [ -e "$LINK" ]; then
    die "$LINK is not a managed symlink"
  fi
  if [ -d "$STORE" ] && [ ! -f "$STORE/.codex-lhc-managed" ]; then
    die "$STORE is not marked as an installer-managed directory"
  fi
  if [ -d "$STORE" ]; then
    rm -rf "$STORE"
  fi
  say "Removed Codex + LHC command '$NAME' and managed packages from $STORE."
  say "User configuration and LHC archives were preserved."
  exit 0
fi

command -v tar >/dev/null 2>&1 || die "tar is required"
if command -v sha256sum >/dev/null 2>&1; then
  sha256_file() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_file() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "sha256sum or shasum is required"
fi

if [ -z "$VERSION" ]; then
  metadata=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest")
  VERSION=$(printf '%s' "$metadata" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\([^"]*\)".*/\1/p' | head -1)
  [ -n "$VERSION" ] || die "could not resolve the latest release"
fi
case "$VERSION" in
  v*) VERSION=${VERSION#v} ;;
esac
case "$VERSION" in
  *[!0-9A-Za-z.+-]*|'') die "invalid version: $VERSION" ;;
esac

case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64) PLATFORM=linux-x86_64 ;;
  *) die "v${VERSION} publishes a prebuilt artifact for Linux x86-64 only; build from source on $(uname -s):$(uname -m)" ;;
esac

ASSET="codex-lhc-v${VERSION}-${PLATFORM}.tar.gz"
BASE="https://github.com/${REPO}/releases/download/v${VERSION}"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/codex-lhc-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

say "Downloading Codex + LHC v${VERSION} (${PLATFORM})..."
if [ -n "$ASSET_DIR" ]; then
  [ -f "$ASSET_DIR/$ASSET" ] || die "candidate directory is missing $ASSET"
  [ -f "$ASSET_DIR/SHA256SUMS" ] || die "candidate directory is missing SHA256SUMS"
  cp "$ASSET_DIR/$ASSET" "${TMP}/${ASSET}"
  cp "$ASSET_DIR/SHA256SUMS" "${TMP}/SHA256SUMS"
else
  command -v curl >/dev/null 2>&1 || die "curl is required"
  curl -fsSL "${BASE}/${ASSET}" -o "${TMP}/${ASSET}"
  curl -fsSL "${BASE}/SHA256SUMS" -o "${TMP}/SHA256SUMS"
fi
expected=$(awk -v name="$ASSET" '$2 == name { print $1 }' "${TMP}/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS does not list $ASSET"
actual=$(sha256_file "${TMP}/${ASSET}")
[ "$actual" = "$expected" ] || die "checksum mismatch for $ASSET"

mkdir -p "$STORE/versions" "$BIN_DIR"
printf '%s\n' 'managed by codex-lhc install.sh' > "$STORE/.codex-lhc-managed"
DEST="$STORE/versions/$VERSION"
STAGE="$STORE/versions/.${VERSION}.tmp.$$"
rm -rf "$STAGE"
mkdir -p "$STAGE"
tar -xzf "${TMP}/${ASSET}" -C "$STAGE"
[ -x "$STAGE/bin/codex" ] || die "release archive is missing bin/codex"
[ -x "$STAGE/bin/codex-code-mode-host" ] || die "release archive is missing bin/codex-code-mode-host"
[ -f "$STAGE/release-manifest.json" ] || die "release archive is missing release-manifest.json"
rm -rf "$DEST"
mv "$STAGE" "$DEST"
ln -sfn "$DEST" "$STORE/current"

if [ -e "$LINK" ] || [ -L "$LINK" ]; then
  if [ ! -L "$LINK" ]; then
    die "$LINK already exists; choose another name with --name"
  fi
  old_target=$(readlink "$LINK")
  case "$old_target" in
    "$STORE"/*) ;;
    *) die "$LINK is not managed by this installer; choose another name with --name" ;;
  esac
fi

old_version="none"
if [ -f "$STORE/installed-version" ]; then
  old_version=$(cat "$STORE/installed-version")
fi
ln -sfn "$STORE/current/bin/codex" "$LINK"
printf '%s\n' "$VERSION" > "$STORE/installed-version"
printf '%s\n' "$NAME" > "$STORE/installed-name"

lhc_pin=$(sed -n 's/.*"lhcSdkCommit"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$DEST/release-manifest.json" | head -1)
say "Installed Codex + LHC: v${old_version} -> v${VERSION}"
[ -z "$lhc_pin" ] || say "LHC engine updated to ${lhc_pin}."
say "Command: $LINK"
say "Package: $DEST"
say "Note: full transcripts and derived views can use substantially more disk than stock Codex."
