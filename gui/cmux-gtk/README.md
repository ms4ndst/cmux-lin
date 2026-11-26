# cmux-gtk (prototype)

A throwaway prototype of a native GTK4 window for a cmux-tui session. It's here to test whether a native Linux frontend is practical, not as a finished app.

The cmux-tui server stays in charge of everything. This window is just another client:

- the sidebar lists the session's workspaces (re-read every 1.5 s);
- clicking a workspace shows its active terminal in a VTE widget (the terminal widget GNOME Terminal uses);
- keystrokes and pastes go back to that terminal;
- resizing the window resizes the terminal on the server, so the shell reflows;
- terminal colours follow the active cmux theme;
- `+` creates a workspace.

## Build and run

Needs `gtk4-devel` and `vte291-gtk4-devel`, plus the upstream source fetched by `scripts/20-fetch-source.sh` (the app uses upstream's Rust SDK from there).

```bash
cd gui/cmux-gtk
cargo run --release -- --session main
```

The session must already be running (`cmux server start --session main`, or just `cmux`). A session with no workspaces shows an empty sidebar until you press `+` or attach the TUI once.

## How it works

Each shown terminal gets its own connection to the session socket, attached in byte mode. The server first sends a replay of the screen, then live output, all as ordinary terminal escape sequences that VTE renders directly. Input (`send`) and size reports (`resize-surface`) go over **that same connection**.

That last part is a protocol rule: the server sizes a terminal from its attached viewers only, treats every connection as a separate viewer, and gives the size to the most recently active one. A resize sent on any other connection comes back `accepted: false`. The Rust SDK's raw attach stream doesn't let you send on its connection, so the attachment code speaks the protocol's newline-delimited JSON directly. The workspace tree and workspace creation still go through the SDK.

The protocol's colour messages carry foreground, background, cursor and selection, but the 16-colour palette only as per-terminal overrides. The theme's base palette comes from the same `~/.config/ghostty/config` block the server reads (written by `cmux-theme`).

## Not done

- splits and multiple tabs per workspace (only the active tab is shown);
- switching workspaces on the server when you click one (the click is local);
- agent attention badges and desktop notifications;
- browser panes;
- reconnecting when the server restarts.

## Debugging

- `CMUX_GTK_SNAPSHOT=/abs/path.png` renders the window to that PNG every 2 s, without screen capture.
- `CMUX_GTK_DEBUG=1` logs size reports to stderr.
