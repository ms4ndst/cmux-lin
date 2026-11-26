# cmux-gtk (prototype)

A throwaway prototype of a native GTK4 window for a cmux-tui session. It's here to test whether a native Linux frontend is practical, not as a finished app.

The cmux-tui server stays in charge of everything. This window is just another client:

- the sidebar lists the session's workspaces (re-read every 1.5 s, and straight after every action);
- the selected workspace's split layout is drawn as resizable panes, and dragging a divider moves it in the TUI too (and the other way round);
- every pane has a tab strip and a VTE terminal (the widget GNOME Terminal uses) for its active tab;
- tab strip: click a tab to switch, `×` to close, `+` for a new tab, `🌐` for a browser tab, `┃`/`━` to split right/down;
- browser tabs show the live page with an address bar and back/forward/reload; clicks, scrolling and typing go to the page (see below);
- keystrokes and pastes go to the pane you're typing in, and focusing a pane makes it the session's active pane;
- resizing the window or a pane resizes that terminal on the server, so the shell reflows;
- terminal colours follow the active cmux theme;
- agent attention shows as badges on tabs and sidebar rows (see below), and notifications pop up on the desktop;
- the header's `+` creates a workspace;
- if the session stops or restarts, the window dims and waits, then reconnects by itself and re-attaches every pane (it can also be started before the session exists).

All layout changes are requests to the server; the window only redraws from the tree the server reports back, so it can't drift from the TUI.

## Browser tabs

A cmux browser tab is a real Chromium page that the session gets from a *browser provider*, so the TUI, this window and agents all see and drive the same page. cmux-tui never runs a browser itself, and upstream's provider (cmux-browser, a patched Chromium) isn't publicly buildable, so this repo has its own:

- **Built in.** The app starts a provider when it connects, unless another one is already registered. When the session has a browser tab, it launches a headless Chromium-family browser, opens a page for every browser tab, and registers them. It takes `CMUX_LIN_BROWSER` if set, otherwise the first of `chromium`, `chromium-browser`, `ungoogled-chromium`, `brave-browser`, `brave`, `microsoft-edge`, `google-chrome-stable`, `google-chrome` on `PATH`. Each browser gets its own profile in `~/.local/share/cmux-lin/browsers/<name>`, so logins persist. Set `CMUX_GTK_NO_PROVIDER=1` to turn it off.
- **Standalone.** `cmux-browser-provider [--session NAME]` does the same without the window, so browser tabs also work in the TUI (which draws them with kitty graphics). It's its own crate, [`browser-provider/`](../../browser-provider), with no GTK dependency, and `scripts/40-install.sh` installs it. Stop it with Ctrl-C or `kill`.

The browser never outlives its provider, even if the app is killed with `kill -9` or crashes. It's started with `--remote-debugging-pipe`, and Chromium exits when that pipe closes, which the kernel does whenever the provider's process ends. A parent-death signal alone isn't enough, because some launchers, Brave's for one, run the real browser as a grandchild.

If no browser can be started, the provider says so once, in the browser tab and the status line (the standalone provider prints it), and retries quietly every 30 seconds.

In the window, a browser tab has an address bar (bare domains get `https://`) plus back, forward and reload. Clicks, drags, scrolling and typing are forwarded to the page. Each click quotes the frame it was aimed at, the protocol's guard against clicking a page that changed underneath you.

## Agent attention and notifications

Each tab gets a mark, ranked most urgent first the same way every cmux client ranks them:

| Mark | Meaning |
|---|---|
| red `●` | an agent is waiting for you (blocked) |
| `✓` | an agent finished a turn you haven't looked at yet |
| `●` in the level's colour | an unread notification (error, warning, info) |
| `◐` | an agent is working |

Each sidebar row shows its workspace's most urgent mark and how many tabs need you. Colours come from the active cmux theme's notification colours.

When a tab with a finished turn or an unread notification is on screen and the window has focus, the app tells the server you've seen it (`ack-tab-notifications`), the same way the TUI does. That clears the mark everywhere, TUI included.

Notifications also go to the desktop notification service (dunst, GNOME Shell and so on), unless you're already looking at that terminal. Clicking one jumps to its tab. The latest notification also shows in the status line for a few seconds.

## Keyboard shortcuts

They act on the pane you're typing in:

| Keys | Action |
|---|---|
| `Ctrl+Shift+T` | New tab |
| `Ctrl+Shift+W` | Close tab |
| `Ctrl+Shift+E` | Split right |
| `Ctrl+Shift+O` | Split down |
| `Ctrl+Shift+B` | New browser tab |
| `Ctrl+PageDown` / `Ctrl+PageUp` | Next / previous tab |

## Requirements

- **Always:** a running cmux-tui session, plus `gtk4-devel` and `vte291-gtk4-devel` to build.
- **Only for browser tabs:** a Chromium-based browser, for example Chromium (`sudo dnf install chromium`), Brave or Microsoft Edge. Google Chrome works too, but it's only used if nothing else is installed, because it reports usage to Google even when run headless. Firefox can't be used: cmux talks to pages over the Chrome DevTools Protocol, which Firefox no longer supports. Without one, everything else works, and a browser tab says what's missing instead of showing a page.

## Build and run

Needs `gtk4-devel` and `vte291-gtk4-devel`, plus the upstream source fetched by `scripts/20-fetch-source.sh` (the app uses upstream's Rust SDK from there).

```bash
cd gui/cmux-gtk
cargo run --release -- --session main
```

The window waits for the session if it isn't running yet (`cmux server start --session main`, or just `cmux`). A session with no workspaces shows an empty sidebar until you press `+` or attach the TUI once.

## How it works

Each pane's visible terminal gets its own connection to the session socket, attached in byte mode. The server first sends a replay of the screen, then live output, all as ordinary terminal escape sequences that VTE renders directly. Input (`send`) and size reports (`resize-surface`) go over **that same connection**.

That last part is a protocol rule: the server sizes a terminal from its attached viewers only, treats every connection as a separate viewer, and gives the size to the most recently active one. A resize sent on any other connection comes back `accepted: false`. The Rust SDK's raw attach stream doesn't let you send on its connection, so the attachment code speaks the protocol's newline-delimited JSON directly. The workspace tree and workspace creation still go through the SDK.

If a tree poll fails, the window closes every attachment, dims itself and blocks input, and the control thread retries the socket every second. When a tree can be read again, each pane re-attaches and gets a fresh screen replay. Pane widgets are reused by pane id, so a restart that keeps the layout doesn't rebuild anything; shells running in the session survive a server restart.

A third connection subscribes to session events. `notification` events become desktop notifications, and tree, agent and notification events trigger an immediate re-poll of the tree and the agent roster (`list-agents`), so badges update within a moment rather than on the next 1.5 s poll.

The protocol's colour messages carry foreground, background, cursor and selection, but the 16-colour palette only as per-terminal overrides. The theme's base palette comes from the same `~/.config/ghostty/config` block the server reads (written by `cmux-theme`).

## Not done

- multiple screens per workspace (only the active screen is shown);
- switching workspaces on the server when you click one (the click is local);
- browser text selection, copy/paste, right-click menus and file uploads (pointer, scroll and typing work);
- zoom, pane swap, and other layout commands beyond split/close;

## Debugging

- `CMUX_GTK_SNAPSHOT=/abs/path.png` renders the window to that PNG every 2 s, without screen capture.
- `CMUX_GTK_DEBUG=1` logs per-pane size reports to stderr.
