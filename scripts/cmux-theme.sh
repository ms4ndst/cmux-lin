#!/usr/bin/env bash
# cmux-theme — theme selector for cmux (cmux-tui) on Linux.
#
#   cmux-theme list              show installed themes, mark the active one
#   cmux-theme current           print the active theme name
#   cmux-theme set <name>        activate a theme from ~/.config/cmux/themes/
#   cmux-theme set default       remove the theme override (upstream defaults)
#   cmux-theme cycle             activate the next theme alphabetically
#   cmux-theme install <file>    add or replace a theme from a JSON file
#   cmux-theme pair <name> <desktop-theme>
#                                pin a desktop theme (~/.config/waybar/themes)
#                                to this theme; switching then re-applies the
#                                whole desktop (waybar/niri/kitty/starship/dunst)
#                                via the local waybar-theme.sh pipeline
#   cmux-theme unpair <name>     remove the desktop pairing again
#
# Theme files are JSON objects of the shape exported in assets/themes:
#   {"theme": { ...theme keys from docs/configuration.md... }}
# An optional "desktop" key names a desktop theme to apply on switch.
# Only the "theme" key of ~/.config/cmux/cmux-tui.json is replaced; every
# other section (commands, sidebar, tabs, ...) is preserved. After writing,
# every running session's config is reloaded so an attached TUI re-themes
# live (cmux server reload-config).
set -euo pipefail

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/cmux"
CONFIG="$CONFIG_DIR/cmux-tui.json"
THEME_DIR="$CONFIG_DIR/themes"
GHOSTTY_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/ghostty/config"
GHOSTTY_BEGIN="# begin cmux-theme managed block (rewritten by cmux-theme)"
GHOSTTY_END="# end cmux-theme managed block"
RUN_DIR="/run/user/$(id -u)/cmux-tui-$(id -u)"

die() { printf 'cmux-theme: %s\n' "$*" >&2; exit 1; }

[ -f "$CONFIG" ] || die "no cmux config at $CONFIG (run scripts/40-install.sh first)"
command -v python3 >/dev/null 2>&1 || die "python3 required"
command -v cmux >/dev/null 2>&1 || die "cmux not on PATH"
mkdir -p "$THEME_DIR"

# List installed themes, one per line.
list_names() {
  local f
  for f in "$THEME_DIR"/*.json; do
    [ -e "$f" ] || return 0
    basename "$f" .json
  done
}

# Print the name of the theme currently in the config.
current_name() {
  python3 - "$CONFIG" "$THEME_DIR" <<'EOF'
import json, os, sys
cfg = json.load(open(sys.argv[1]))
if 'theme' not in cfg:
    print('default'); sys.exit()
live = json.dumps(cfg['theme'], sort_keys=True)
for f in sorted(os.listdir(sys.argv[2])):
    if f.endswith('.json'):
        t = json.load(open(os.path.join(sys.argv[2], f)))
        if json.dumps(t.get('theme'), sort_keys=True) == live:
            print(f[:-5]); sys.exit()
print('custom')
EOF
}

# Write the theme section and, when the theme carries one, the pane-content
# defaults (background/foreground/cursor/selection/ANSI palette) which
# cmux-tui reads from the Ghostty config. "default" clears both.
# $1 = theme name or "default".
apply_theme() {
  python3 - "$CONFIG" "$THEME_DIR" "$1" "$GHOSTTY_CONFIG" "$GHOSTTY_BEGIN" "$GHOSTTY_END" <<'EOF'
import json, os, re, sys

cfg_path, theme_dir, name, g_path, g_begin, g_end = sys.argv[1:7]

try:
    cfg = json.load(open(cfg_path))
except (OSError, ValueError) as e:
    sys.exit(f"cmux-theme: cannot parse {cfg_path}: {e}")

if name == 'default':
    cfg.pop('theme', None)
    ghostty = None
else:
    tpath = os.path.join(theme_dir, name + '.json')
    if not os.path.isfile(tpath):
        sys.exit(f"cmux-theme: no such theme: {name}")
    theme_file = json.load(open(tpath))
    theme = theme_file.get('theme')
    if not isinstance(theme, dict):
        sys.exit(f"cmux-theme: {tpath} has no \"theme\" object")
    cfg['theme'] = theme
    ghostty = theme_file.get('ghostty')

with open(cfg_path, 'w') as fh:
    json.dump(cfg, fh, indent=2)
    fh.write('\n')

# Managed block in the Ghostty config for pane-content defaults.
lines = []
if isinstance(ghostty, dict):
    lines.append(g_begin)
    for key in ('background', 'foreground', 'cursor-color',
                'selection-background', 'selection-foreground'):
        if key in ghostty:
            lines.append(f'{key} = {ghostty[key]}')
    for idx in sorted(ghostty.get('palette', {}), key=int):
        lines.append(f'palette = {idx}={ghostty["palette"][idx]}')
    lines.append(g_end)

existing = ''
if os.path.isfile(g_path):
    existing = open(g_path).read()
pattern = re.compile(re.escape(g_begin) + r'.*?' + re.escape(g_end) + r'\n?',
                    re.DOTALL)
existing = pattern.sub('', existing)
if lines:
    block = '\n'.join(lines) + '\n'
    existing = existing.rstrip('\n') + ('\n\n' if existing.strip() else '') + block
os.makedirs(os.path.dirname(g_path), exist_ok=True)
with open(g_path, 'w') as fh:
    fh.write(existing)
EOF
}

# Live pane re-theming is native: the ghostty config block feeds cmux's
# terminal_defaults, and the local cmux-lin patch pushes those defaults
# through the mux on every config reload (set/cycle reload each running
# session), which repaints all live surfaces and sets the defaults for
# future panes. Nothing is written into pane PTYs here.

# After a theme is applied, honor an optional "desktop" key in the theme
# file: the name of a desktop theme directory in ~/.config/waybar/themes.
# When present and installed, cmux re-applies it via waybar-theme.sh, which
# retints the whole desktop (waybar, niri focus-ring, kitty, starship, dunst).
desktop_sync() {
  local theme_file="$THEME_DIR/$1.json"
  [ -f "$theme_file" ] || return 0
  local desktop
  desktop="$(python3 -c "import json,sys;print(json.load(open(sys.argv[1])).get('desktop',''))" "$theme_file" 2>/dev/null || true)"
  [ -n "$desktop" ] || return 0
  command -v waybar-theme.sh >/dev/null 2>&1 || { printf 'desktop pairing "%s" ignored: no waybar-theme.sh\n' "$desktop"; return 0; }
  [ -d "${WAYBAR_THEMES_DIR:-$HOME/.config/waybar/themes}/$desktop" ] || { printf 'desktop pairing "%s" ignored: not installed\n' "$desktop"; return 0; }
  if waybar-theme.sh "$desktop" >/dev/null 2>&1; then
    printf 'desktop theme applied: %s\n' "$desktop"
  else
    printf 'desktop pairing "%s" failed: waybar-theme.sh exited nonzero\n' "$desktop"
  fi
}

# Reload every running session so attached TUIs re-theme immediately.
reload_running() {
  local sock session reloaded=0
  [ -d "$RUN_DIR" ] || return 0
  for sock in "$RUN_DIR"/*.sock; do
    [ -S "$sock" ] || continue
    session="$(basename "$sock" .sock)"
    if cmux server reload-config --session "$session" >/dev/null 2>&1; then
      printf 'reloaded session %s\n' "$session"
      reloaded=1
    fi
  done
  [ "$reloaded" -eq 0 ] && printf 'no running sessions to reload\n'
  return 0
}

cmd="${1:-}"
case "$cmd" in
  list)
    cur="$(current_name)"
    for n in $(list_names | sort); do
      [ "$n" = "$cur" ] && printf '* %s\n' "$n" || printf '  %s\n' "$n"
    done
    [ "$cur" = "default" ] || [ "$cur" = "custom" ] && printf '(%s)\n' "$cur" ||:
    ;;
  current)
    current_name
    ;;
  set)
    [ -n "${2:-}" ] || die "usage: cmux-theme set <name>"
    apply_theme "$2"
    printf 'theme set: %s\n' "$2"
    reload_running
    if [ "$2" != "default" ]; then desktop_sync "$2"; fi
    ;;
  pair)
    [ -n "${2:-}" ] && [ -n "${3:-}" ] || die "usage: cmux-theme pair <name> <desktop-theme>"
    f="$THEME_DIR/$2.json"
    [ -f "$f" ] || die "no such theme: $2"
    [ -d "${WAYBAR_THEMES_DIR:-$HOME/.config/waybar/themes}/$3" ] || die "no such desktop theme: $3"
    python3 - "$f" "$3" <<'EOF'
import json, sys
p, desktop = sys.argv[1], sys.argv[2]
d = json.load(open(p))
d['desktop'] = desktop
json.dump(d, open(p, 'w'), indent=2)
open(p, 'a').write('\n')
EOF
    printf 'paired %s -> %s\n' "$2" "$3"
    ;;
  unpair)
    [ -n "${2:-}" ] || die "usage: cmux-theme unpair <name>"
    f="$THEME_DIR/$2.json"
    [ -f "$f" ] || die "no such theme: $2"
    python3 - "$f" <<'EOF'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d.pop('desktop', None)
json.dump(d, open(p, 'w'), indent=2)
open(p, 'a').write('\n')
EOF
    printf 'unpaired %s\n' "$2"
    ;;
  cycle)
    # Cycle in a perceptually ordered sequence so consecutive themes differ
    # visibly: the three Mocha accents are near-twins of each other, so the
    # light theme breaks them up; everything else follows alphabetical order.
    mapfile -t all < <(list_names | sort)
    [ "${#all[@]}" -gt 0 ] || die "no themes installed in $THEME_DIR"
    ordered=()
    for p in mocha-blue latte-light mocha-mauve mocha-contrast; do
      for n in "${all[@]}"; do
        [ "$n" = "$p" ] && ordered+=("$p")
      done
    done
    for n in "${all[@]}"; do
      case " mocha-blue latte-light mocha-mauve mocha-contrast " in
        *" $n "*) ;;
        *) ordered+=("$n") ;;
      esac
    done
    cur="$(current_name)"
    idx=-1
    for i in "${!ordered[@]}"; do
      [ "${ordered[$i]}" = "$cur" ] && idx="$i"
    done
    if [ "$idx" -lt 0 ]; then
      nexti=0
    else
      nexti=$(( (idx + 1) % ${#ordered[@]} ))
    fi
    next="${ordered[$nexti]}"
    apply_theme "$next"
    printf 'theme set: %s\n' "$next"
    reload_running
    desktop_sync "$next"
    ;;
  install)
    [ -n "${2:-}" ] || die "usage: cmux-theme install <file>"
    [ -f "$2" ] || die "no such file: $2"
    name="$(basename "$2" .json)"
    install -m 0644 "$2" "$THEME_DIR/$name.json"
    printf 'installed theme: %s\n' "$name"
    ;;
  *)
    sed -n '3,22p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
