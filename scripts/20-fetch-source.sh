#!/usr/bin/env bash
# Fetch the pinned cmux source and its ghostty submodule into build/cmux.
source "$(dirname "$0")/common.sh"

mkdir -p "$BUILD_DIR"

if [ -d "$SRC_DIR/.git" ]; then
  current="$(git -C "$SRC_DIR" rev-parse HEAD)"
  if [ "$current" = "$CMUX_COMMIT" ]; then
    log "source already fetched at pinned commit $CMUX_COMMIT"
  else
    log "source exists at $current; resetting to pinned commit $CMUX_COMMIT"
    git -C "$SRC_DIR" fetch origin "$CMUX_COMMIT" --depth 1
    git -C "$SRC_DIR" checkout --detach "$CMUX_COMMIT"
  fi
else
  log "fetching $CMUX_REPO at $CMUX_COMMIT (shallow)"
  # Try local alternates to speed up repeat fetches (ignored if missing).
  git clone --depth 1 "$CMUX_REPO" "$SRC_DIR"
  git -C "$SRC_DIR" fetch origin "$CMUX_COMMIT" --depth 1
  git -C "$SRC_DIR" checkout --detach "$CMUX_COMMIT"
fi

log "initializing ghostty submodule"
git -C "$SRC_DIR" submodule update --init --depth 1 ghostty

sub_commit="$(git -C "$SRC_DIR/ghostty" rev-parse HEAD)"
if [ "$sub_commit" != "$GHOSTTY_COMMIT" ]; then
  die "ghostty submodule at $sub_commit, expected pin $GHOSTTY_COMMIT (PINS.env is stale?)"
fi
log "ghostty submodule at pinned commit $GHOSTTY_COMMIT"
log "source ready: $SRC_DIR"
