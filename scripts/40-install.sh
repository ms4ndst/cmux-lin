#!/usr/bin/env bash
# Install the built cmux-tui binaries into ~/.local/bin and expose them as
# `cmux` (the Linux port of the cmux CLI surface).
source "$(dirname "$0")/common.sh"

BIN="$SRC_DIR/cmux-tui/target/release"
[ -x "$BIN/cmux-tui" ] || die "no release binary; run scripts/30-build.sh"

DEST="${CMUX_LIN_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$DEST"

install -m 0755 "$BIN/cmux-tui" "$DEST/cmux-tui"
install -m 0755 "$BIN/cmux-tui-hook" "$DEST/cmux-tui-hook"
ln -sfn "$DEST/cmux-tui" "$DEST/cmux"

# Browser provider: browser tabs need one, and upstream's (cmux-browser)
# isn't available on Linux. Built by 30-build.sh; optional at runtime (it
# needs a Chromium-family browser, only when a browser tab exists).
PROVIDER="$REPO_ROOT/browser-provider/target/release/cmux-browser-provider"
if [ -x "$PROVIDER" ]; then
  install -m 0755 "$PROVIDER" "$DEST/cmux-browser-provider"
else
  log "cmux-browser-provider not built (run scripts/30-build.sh); browser tabs will need another provider"
fi

CONFIG_SRC="$REPO_ROOT/assets/cmux-tui.json"
CONFIG_DEST="${XDG_CONFIG_HOME:-$HOME/.config}/cmux/cmux-tui.json"
if [ -f "$CONFIG_SRC" ]; then
  if [ -e "$CONFIG_DEST" ]; then
    log "theme config already present at $CONFIG_DEST (left untouched)"
  else
    mkdir -p "$(dirname "$CONFIG_DEST")"
    install -m 0644 "$CONFIG_SRC" "$CONFIG_DEST"
    log "installed Catppuccin Mocha theme config to $CONFIG_DEST"
  fi
fi

# cmux-theme selector + shipped themes. Shipped theme files are refreshed;
# any custom extra themes in the directory are left alone.
install -m 0755 "$REPO_ROOT/scripts/cmux-theme.sh" "$DEST/cmux-theme"
THEME_DEST="${XDG_CONFIG_HOME:-$HOME/.config}/cmux/themes"
mkdir -p "$THEME_DEST"
for t in "$REPO_ROOT"/assets/themes/*.json; do
  install -m 0644 "$t" "$THEME_DEST/$(basename "$t")"
done
log "cmux-theme installed with themes: $(ls "$THEME_DEST" | sed 's/\.json//' | tr '\n' ' ')"

# In-TUI theme picker: one command + menu entry per installed theme on the
# tab bar's `+` button (right-click). Install into the seed asset and the
# live user config; user-added commands are preserved.
python3 "$REPO_ROOT/scripts/install-theme-picker.py" "$REPO_ROOT/assets/cmux-tui.json" "$CONFIG_DEST"

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) log "NOTE: $DEST is not on PATH; add it to your shell profile." ;;
esac

log "installed:"
ls -l "$DEST/cmux-tui" "$DEST/cmux-tui-hook" "$DEST/cmux"
[ -x "$DEST/cmux-browser-provider" ] && ls -l "$DEST/cmux-browser-provider"
log "cmux-tui $("$DEST/cmux-tui" --version | head -1)"
