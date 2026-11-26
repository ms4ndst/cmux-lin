#!/usr/bin/env python3
"""Generate cmux themes from scrollwerk's Android palette definitions.

Parses NamedPalette entries in scrollwerk's Palettes.kt and emits one cmux
theme file per palette variant:

  assets/themes/sw-<slug>-<dark|light>.json   {"theme": {...}, "ghostty": {...}}

"theme"   drives cmux-owned chrome (sidebar, tabs, borders, status bar,
          notification accents) via cmux-tui.json.
"ghostty" drives the hosted terminals' default colors: background,
          foreground, cursor, selection, and the 16-color ANSI palette
          (cmux-tui reads ~/.config/ghostty/config as application defaults).

Layering honors the requested elevation: the pane background is the palette's
surfaceContainer, one notch lighter than the cmux base layer (surface).

ANSI slots are mapped by Noctalia role, not by hue:
  0/8 grays     -> surfaceContainer / outlineVariant
  1/9 red       -> error
  2/10 green    -> secondary
  3/11 yellow   -> primary
  4/12 blue     -> hover
  5/13 magenta  -> tertiary
  6/14 cyan     -> hover
  7/15 whites   -> onSurface / onSurfaceVariant
"""
import json
import re
import sys
from pathlib import Path

if len(sys.argv) < 2:
    sys.exit(f"usage: {sys.argv[0]} <path/to/scrollwerk/.../ui/theme/Palettes.kt> [out_dir]")
SRC = Path(sys.argv[1])
OUT = Path(sys.argv[2] if len(sys.argv) > 2 else
           Path(__file__).resolve().parent.parent / "assets/themes")

COLOR_RE = re.compile(r"Color\(0x([0-9A-Fa-f]{8})\)")


def parse_colors(blob: str) -> dict:
    colors = {}
    for field, value in re.findall(r"(\w+) = Color\(0x([0-9A-Fa-f]{8})\)", blob):
        colors[field] = "#" + value[2:].upper()
    return colors


def hexify(rgb_hex: str) -> str:
    return "#" + rgb_hex.lstrip("#").upper()


def build(nx: dict, chrome: str) -> dict:
    return {
        "theme": {
            "chrome": chrome,
            "border_style": "rounded",
            "border_active": nx["primary"],
            "border_inactive": nx["outlineVariant"],
            "notification_info": nx["primary"],
            "notification_warning": nx["secondary"],
            "notification_error": nx["error"],
            "sidebar_rail": nx["primary"],
            "sidebar_active_bg": nx["surfaceContainer"],
            "sidebar_fg": nx["onSurfaceVariant"],
            "sidebar_selected_fg": nx["onSurface"],
            "tab_rail": nx["primary"],
            "tab_bg": nx["surface"],
            "tab_active_bg": nx["surfaceContainer"],
            "status_bg": nx["surface"],
            "status_fg": nx["onSurfaceVariant"],
            "selection_background": nx["outlineVariant"],
            "menu_bg": nx["surfaceContainer"],
            "menu_fg": nx["onSurface"],
            "menu_border": nx["outlineVariant"],
            "menu_selected_bg": nx["hover"],
            "menu_selected_fg": nx["onHover"],
            "prompt_bg": nx["surfaceContainer"],
            "prompt_fg": nx["onSurface"],
            "prompt_border": nx["outlineVariant"],
            "toast_bg": nx["surfaceContainer"],
            "toast_fg": nx["onSurface"],
            "status_active_bg": nx["surfaceContainer"],
            "status_active_fg": nx["onSurface"],
            "status_dim_fg": nx["onSurfaceVariant"],
            "tab_bar_bg": nx["surface"],
            "sidebar_selected_bg": nx["surfaceContainer"],
            "sidebar_border": nx["outlineVariant"],
            "sidebar_dim_fg": nx["onSurfaceVariant"],
            "dim_inactive": True,
        },
        "ghostty": {
            "background": nx["surfaceContainer"],
            "foreground": nx["onSurface"],
            "cursor-color": nx["primary"],
            "selection-background": nx["outlineVariant"],
            "selection-foreground": nx["onSurface"],
            "palette": {
                "0": nx["surfaceContainer"], "8": nx["outlineVariant"],
                "1": nx["error"],          "9": nx["error"],
                "2": nx["secondary"],      "10": nx["secondary"],
                "3": nx["primary"],        "11": nx["primary"],
                "4": nx["hover"],          "12": nx["hover"],
                "5": nx["tertiary"],       "13": nx["tertiary"],
                "6": nx["hover"],          "14": nx["hover"],
                "7": nx["onSurface"],      "15": nx["onSurfaceVariant"],
            },
        },
    }


def slugify(name: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")


def parse_block(chunk: str, marker: str) -> str:
    """Return the inner text of the NoctaliaColors(...) after `marker`,
    balanced against the nested Color(...) parens."""
    start = chunk.index(marker) + len(marker)
    depth = 1
    for i in range(start, len(chunk)):
        if chunk[i] == "(":
            depth += 1
        elif chunk[i] == ")":
            depth -= 1
            if depth == 0:
                return chunk[start:i]
    raise SystemExit(f"unbalanced parens after {marker!r}")


def main() -> None:
    chunks = SRC.read_text().split("NamedPalette(")[1:]
    OUT.mkdir(parents=True, exist_ok=True)
    count = 0
    for chunk in chunks:
        name_match = re.search(r'"([^"]+)"', chunk)
        if not name_match or "dark = NoctaliaColors(" not in chunk \
                or "light = NoctaliaColors(" not in chunk:
            continue  # class declaration or fragment, not a palette entry
        name = name_match.group(1)
        for variant, chrome in (("dark", "dark"), ("light", "light")):
            nx = parse_colors(parse_block(chunk, f"{variant} = NoctaliaColors("))
            missing = [k for k in ("primary", "secondary", "tertiary", "error", "surface",
                                   "onSurface", "surfaceContainer", "onSurfaceVariant",
                                   "outlineVariant", "hover") if k not in nx]
            if missing:
                sys.exit(f"{name}/{variant}: missing colors {missing}; parser drift?")
            theme = build(nx, chrome)
            path = OUT / f"sw-{slugify(name)}-{variant}.json"
            path.write_text(json.dumps(theme, indent=2) + "\n")
            count += 1
            print(f"{path.name}: accent {nx['primary']}, pane bg {nx['surfaceContainer']}")
    print(f"generated {count} themes into {OUT}")


if __name__ == "__main__":
    main()
