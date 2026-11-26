# cmux-gtk (prototype)

A throwaway prototype of a native GTK4 window for a cmux-tui session. It's here to test whether a native Linux frontend is practical, not as a finished app.

The cmux-tui server stays in charge of everything. This window is just another client:

- the sidebar lists the session's workspaces (re-read every 1.5 s, and straight after every action);
- the selected workspace's split layout is drawn as resizable panes, and dragging a divider moves it in the TUI too (and the other way round);
- every pane has a tab strip and a VTE terminal (the widget GNOME Terminal uses) for its active tab;
- tab strip: click a tab to switch, `×` to close, `+` for a new tab, `┃`/`━` to split right/down;
- keystrokes and pastes go to the pane you're typing in, and focusing a pane makes it the session's active pane;
- resizing the window or a pane resizes that terminal on the server, so the shell reflows;
- terminal colours follow the active cmux theme;
- the header's `+` creates a workspace.

All layout changes are requests to the server; the window only redraws from the tree the server reports back, so it can't drift from the TUI.

## Keyboard shortcuts

They act on the pane you're typing in:

| Keys | Action |
|---|---|
| `Ctrl+Shift+T` | New tab |
| `Ctrl+Shift+W` | Close tab |
| `Ctrl+Shift+E` | Split right |
| `Ctrl+Shift+O` | Split down |
| `Ctrl+PageDown` / `Ctrl+PageUp` | Next / previous tab |

## Build and run

Needs `gtk4-devel` and `vte291-gtk4-devel`, plus the upstream source fetched by `scripts/20-fetch-source.sh` (the app uses upstream's Rust SDK from there).

```bash
cd gui/cmux-gtk
cargo run --release -- --session main
```

The session must already be running (`cmux server start --session main`, or just `cmux`). A session with no workspaces shows an empty sidebar until you press `+` or attach the TUI once.

## How it works

Each pane's visible terminal gets its own connection to the session socket, attached in byte mode. The server first sends a replay of the screen, then live output, all as ordinary terminal escape sequences that VTE renders directly. Input (`send`) and size reports (`resize-surface`) go over **that same connection**.

That last part is a protocol rule: the server sizes a terminal from its attached viewers only, treats every connection as a separate viewer, and gives the size to the most recently active one. A resize sent on any other connection comes back `accepted: false`. The Rust SDK's raw attach stream doesn't let you send on its connection, so the attachment code speaks the protocol's newline-delimited JSON directly. The workspace tree and workspace creation still go through the SDK.

The protocol's colour messages carry foreground, background, cursor and selection, but the 16-colour palette only as per-terminal overrides. The theme's base palette comes from the same `~/.config/ghostty/config` block the server reads (written by `cmux-theme`).

## Not done

- multiple screens per workspace (only the active screen is shown);
- switching workspaces on the server when you click one (the click is local);
- browser tabs (listed in the tab strip, but not rendered);
- zoom, pane swap, and other layout commands beyond split/close;
- agent attention badges and desktop notifications;
- reconnecting when the server restarts.

## Debugging

- `CMUX_GTK_SNAPSHOT=/abs/path.png` renders the window to that PNG every 2 s, without screen capture.
- `CMUX_GTK_DEBUG=1` logs per-pane size reports to stderr.
