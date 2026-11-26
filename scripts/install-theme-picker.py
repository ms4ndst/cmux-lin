#!/usr/bin/env python3
"""Install the in-TUI theme picker into a cmux-tui config.

Adds one user command per installed theme (id theme_set_<name>, running
`cmux-theme set <name>`) and lists them in the tab-bar `+` button's
right-click menu (`tabs.plus.menu`, sidebar-action grammar with labels).
Existing user commands other than theme_set_* are preserved; the menu is
owned by the installer and replaced wholesale on each run.

Also ensures the `quit-session` command (prefix + Q): it stops the server of
the session it is pressed in, ending every shell, and the attached TUI exits.
Plain detach (prefix + d) keeps the session alive as before. Added only when
no command with that id exists, so a user's own edits to it are kept.

Usage: install-theme-picker.py <config.json> [<config2.json> ...]
"""
import json
import sys
from pathlib import Path

THEME_DIR = Path.home() / ".config" / "cmux" / "themes"

# CMUX_TUI_SOCKET is inherited by every command child, so this stops the
# session the key was pressed in, whatever its name.
QUIT_COMMAND = {
    "id": "quit-session",
    "name": "Quit session (stop server)",
    "keys": "Q",
    "run": ["sh", "-c", 'exec cmux server stop --socket "$CMUX_TUI_SOCKET"'],
}


def pretty(name: str) -> str:
    if name == "default":
        return "Default (upstream)"
    words = name.replace("-dark", " dark").replace("-light", " light")
    return " ".join(w[:1].upper() + w[1:] for w in words.split())


def build_entry(name: str) -> dict:
    return {"action": f"command:theme_set_{name}", "label": pretty(name)}


def inject(config_path: Path) -> None:
    config = json.loads(config_path.read_text())

    names = (
        sorted(p.stem for p in THEME_DIR.glob("*.json")) + ["default"]
        if THEME_DIR.is_dir() else ["default"]
    )

    commands = [c for c in config.get("commands", [])
                if not str(c.get("id", "")).startswith("theme_set_")]
    commands.extend(
        {
            "id": f"theme_set_{name}",
            "name": f"Theme: {pretty(name)}",
            "run": ["sh", "-c", f"cmux-theme set {name}"],
        }
        for name in names
    )
    if not any(c.get("id") == QUIT_COMMAND["id"] for c in commands):
        commands.insert(0, QUIT_COMMAND)
    config["commands"] = commands

    tabs = config.setdefault("tabs", {})
    plus = tabs.setdefault("plus", {})
    plus["menu"] = [build_entry(n) for n in names]

    config_path.write_text(json.dumps(config, indent=2) + "\n")
    print(f"[cmux-lin] theme picker injected into {config_path} "
          f"({len(names)} menu entries)")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(f"usage: {sys.argv[0]} <config.json> [...]")
    for arg in sys.argv[1:]:
        inject(Path(arg))
