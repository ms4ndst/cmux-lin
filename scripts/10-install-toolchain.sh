#!/usr/bin/env bash
# Install the toolchain needed to build cmux-tui on Linux:
# rust (via rustup), Zig at the pinned version, and prerequisite checks.
source "$(dirname "$0")/common.sh"

check_cmd git "dnf install git"
check_cmd curl "dnf install curl"
check_cmd gcc "dnf install gcc"

# --- Rust -------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  log "cargo not found; installing rustup ($RUST_TOOLCHAIN, minimal profile)"
  curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain "$RUST_TOOLCHAIN" --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
else
  log "cargo already installed: $(cargo --version)"
fi
check_cmd cargo "rustup install failed; see ~/.cargo"

# Zig must be invocable through PATH later, so keep ~/.local/bin or add zig dir.
# We do not modify PATH persistently; 30-build.sh sets PATH per-invocation.

# --- Zig --------------------------------------------------------------------
if [ -x "$ZIG_BIN" ] && "$ZIG_BIN" version | grep -qx "$ZIG_VERSION"; then
  log "Zig $ZIG_VERSION already installed at $ZIG_INSTALL_DIR"
else
  log "installing Zig $ZIG_VERSION into $HOME/opt/"
  mkdir -p "$HOME/opt"
  tmp="$(mktemp -d)"
  curl -sLo "$tmp/zig.tar.xz" "$ZIG_TARBALL"
  echo "$ZIG_SHA256  $tmp/zig.tar.xz" | sha256sum -c -
  tar -xJf "$tmp/zig.tar.xz" -C "$HOME/opt"
  rm -rf "$tmp"
  "$ZIG_BIN" version | grep -qx "$ZIG_VERSION" || die "installed Zig does not report $ZIG_VERSION"
  log "Zig $ZIG_VERSION installed"
fi

# --- libclang (bindgen) -------------------------------------------------------
if [ ! -e /usr/lib64/libclang.so.1 ] && ! ls /usr/lib64/llvm*/lib64/libclang.so* >/dev/null 2>&1; then
  die "libclang not found; run: sudo dnf install clang-devel"
fi
log "libclang present"

# gcc companion headers for bindgen (limits.h include_next target)
GCC_INCLUDE_DIR="$(dirname "$(gcc -print-libgcc-file-name)")/include"
[ -f "$GCC_INCLUDE_DIR/limits.h" ] || die "gcc include dir missing limits.h ($GCC_INCLUDE_DIR); dnf install gcc"
log "gcc companion headers at $GCC_INCLUDE_DIR"

log "toolchain ready"
