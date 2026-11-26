#!/usr/bin/env bash
# Build cmux-tui from the fetched source. Handles the three Linux-specific
# environment issues documented in README.md -> "Linux fixes".
source "$(dirname "$0")/common.sh"

[ -d "$SRC_DIR/cmux-tui" ] || die "source not fetched; run scripts/20-fetch-source.sh"
[ -x "$ZIG_BIN" ] || die "Zig not installed; run scripts/10-install-toolchain.sh"
check_cmd cargo "run scripts/10-install-toolchain.sh"
check_cmd gcc "dnf install gcc"

# Local patches on top of the pinned upstream commit.
for p in "$REPO_ROOT"/patches/*.patch; do
  [ -e "$p" ] || continue
  if git -C "$SRC_DIR" apply --check "$p" 2>/dev/null; then
    git -C "$SRC_DIR" apply "$p"
    log "applied patch $(basename "$p")"
  else
    log "patch $(basename "$p") already applied or not applicable (skipping)"
  fi
done

# 1. Zig must be reachable via PATH as well as $ZIG (ghostty's build.zig
#    shells out to `zig env` through PATH).
# 2. A stale LD_LIBRARY_PATH (e.g. a VS Code extension bundling an old
#    libstdc++) breaks libclang loading in bindgen; unset it.
# 3. bindgen's clang needs gcc's companion headers for limits.h include_next.
ZIG_DIR="$(dirname "$ZIG_BIN")"
GCC_INCLUDE_DIR="$(dirname "$(gcc -print-libgcc-file-name)")/include"

log "building cmux-tui (release)"
cd "$SRC_DIR/cmux-tui"
env -u LD_LIBRARY_PATH \
  PATH="$HOME/.cargo/bin:$ZIG_DIR:$PATH" \
  ZIG="$ZIG_BIN" \
  BINDGEN_EXTRA_CLANG_ARGS="-I$GCC_INCLUDE_DIR" \
  cargo build --release -p cmux-tui

# Linux browser provider (this repo's own crate; needs only the SDK from the
# fetched source, no GTK): makes browser tabs work without cmux-browser.
log "building cmux-browser-provider (release)"
env -u LD_LIBRARY_PATH \
  PATH="$HOME/.cargo/bin:$PATH" \
  cargo build --release --manifest-path "$REPO_ROOT/browser-provider/Cargo.toml"

log "binaries:"
ls -lh "$SRC_DIR/cmux-tui/target/release/cmux-tui" "$SRC_DIR/cmux-tui/target/release/cmux-tui-hook" \
  "$REPO_ROOT/browser-provider/target/release/cmux-browser-provider"
log "build OK"
