#!/usr/bin/env bash
# Shared helpers for cmux-lin build scripts.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../PINS.env
source "$REPO_ROOT/PINS.env"

BUILD_DIR="${CMUX_LIN_BUILD_DIR:-$REPO_ROOT/build}"
SRC_DIR="$BUILD_DIR/cmux"
ZIG_BIN="$ZIG_INSTALL_DIR/zig"

log() { printf '[cmux-lin] %s\n' "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }

check_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' not found on PATH. $2"
}
