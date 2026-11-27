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

# GTK app (if built): binary plus a launcher entry. The desktop file is
# named after the app id (dev.cmuxlin.CmuxGtk.<session>), which desktop
# notifications refer to.
GTK_APP="$REPO_ROOT/gui/cmux-gtk/target/release/cmux-gtk"
if [ -x "$GTK_APP" ]; then
  install -m 0755 "$GTK_APP" "$DEST/cmux-gtk"
  APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
  mkdir -p "$APPS_DIR"
  # The app id is per session; the launcher opens "main".
  rm -f "$APPS_DIR/dev.cmuxlin.CmuxGtk.desktop"  # pre-per-session name
  cat > "$APPS_DIR/dev.cmuxlin.CmuxGtk.main.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=cmux
GenericName=Terminal multiplexer
Comment=Native window for the cmux session "main"
Exec=$DEST/cmux-gtk --session main
Icon=utilities-terminal
Terminal=false
Categories=System;TerminalEmulator;
Keywords=terminal;multiplexer;agents;tmux;
StartupWMClass=dev.cmuxlin.CmuxGtk.main
DESKTOP
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$APPS_DIR" || true
  log "installed cmux-gtk and its launcher entry ($APPS_DIR/dev.cmuxlin.CmuxGtk.main.desktop)"
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

# Shell completions for cmux / cmux-tui / cmux-theme / cmux-gtk, generated
# from the installed cmux-tui's own help so they match the pinned revision.
COMP_TMP="$(mktemp -d)"
if python3 "$REPO_ROOT/scripts/gen-completions.py" "$DEST/cmux-tui" "$COMP_TMP" >/dev/null; then
  BASH_COMP="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions"
  ZSH_COMP="${XDG_DATA_HOME:-$HOME/.local/share}/zsh/site-functions"
  FISH_COMP="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions"
  mkdir -p "$BASH_COMP" "$ZSH_COMP" "$FISH_COMP"
  # bash-completion loads a file by command name: one copy per command.
  for name in cmux cmux-tui cmux-theme cmux-gtk; do
    install -m 0644 "$COMP_TMP/bash/cmux" "$BASH_COMP/$name"
    install -m 0644 "$COMP_TMP/fish/$name.fish" "$FISH_COMP/$name.fish"
  done
  install -m 0644 "$COMP_TMP/zsh/_cmux" "$ZSH_COMP/_cmux"
  log "shell completions installed (bash, fish; zsh: add $ZSH_COMP to fpath)"
else
  log "couldn't generate shell completions (skipped)"
fi
rm -rf -- "$COMP_TMP"

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) log "NOTE: $DEST is not on PATH; add it to your shell profile." ;;
esac

log "installed:"
ls -l "$DEST/cmux-tui" "$DEST/cmux-tui-hook" "$DEST/cmux"
[ -x "$DEST/cmux-browser-provider" ] && ls -l "$DEST/cmux-browser-provider"
[ -x "$DEST/cmux-gtk" ] && ls -l "$DEST/cmux-gtk"
log "cmux-tui $("$DEST/cmux-tui" --version | head -1)"
