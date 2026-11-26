# cmux-lin

Reproducible Linux build of **cmux** — [manaflow-ai/cmux](https://github.com/manaflow-ai/cmux) — as a terminal multiplexer for AI coding agents, built from upstream's Rust component `cmux-tui`.

> Unofficial. All of the multiplexer is upstream cmux by [manaflow-ai](https://github.com/manaflow-ai), built on [Ghostty](https://github.com/ghostty-org/ghostty); see [Credits](#credits).

cmux upstream distributes a native macOS app (Swift/AppKit on libghostty). That GUI does not exist for Linux and cannot be ported in a build script. The part that *does* run on Linux is `cmux-tui`: the Rust multiplexer that keeps machines, sessions, workspaces, screens, panes, tabs, terminals, and browsers, with a sidebar, agent hooks, and notifications. This repo pins the exact upstream commits and rebuilds that component on Linux with three environment fixes (see [Linux fixes](#linux-fixes)).

Status on Fedora 44 (x86_64, kernel 6.x+, verified 2026-11-25):

- `cargo build -p cmux-tui` succeeds (Zig 0.16.0 builds `libghostty-vt.a`, the VT parser core of Ghostty).
- `cmux-tui server start` runs a durable headless session with a control socket.
- `cmux-tui attach` renders the full TUI (sidebar, pane borders, live PTYs) and restores persisted session state across restarts.
- `cmux-tui workspace create`, `screen create`, `terminal list`, and the rest of the noun-first CLI work against the running server.

What this repo builds and what it does not:

| Layer | Included here | Notes |
|---|---|---|
| cmux-tui multiplexer (Rust + libghostty-vt) | yes | headless server, TUI attach, CLI, agent hooks, remote/machine-agent |
| macOS app (Swift/AppKit) | no | requires a GTK4 rewrite of the whole GUI |
| Swift `CLI` that drives the macOS app | no | the Rust CLI here covers the same protocol surface |
| Browser panes (CDP) | builds, untested | needs a local Chrome/Chromium; see `cmux-tui/docs/browser-panes.md` |

## Layout

```
cmux-lin/
├── PINS.env                  # upstream commit pins + toolchain versions
├── scripts/
│   ├── 10-install-toolchain.sh   # rustup (stable) + Zig 0.16.0 + prereq checks
│   ├── 20-fetch-source.sh         # clone cmux at pin, init ghostty submodule
│   ├── 30-build.sh               # cargo build -p cmux-tui (release)
│   └── 40-install.sh             # install cmux-tui + `cmux` symlink to ~/.local/bin
├── browser-provider/         # cmux-browser-provider: makes browser tabs work on Linux
├── gui/cmux-gtk/             # prototype native GTK4 window for a session (see its README)
└── build/                    # (gitignored) source checkout + target/
```

`gui/cmux-gtk` is a native Linux window (GTK4 + VTE) for a running session: workspaces, screens, splits, tabs, browser tabs, agent badges and notifications, driven over cmux's protocol. It complements the TUI rather than replacing it; see [gui/cmux-gtk/README.md](gui/cmux-gtk/README.md).

## Build

Prerequisites (Fedora): `git`, `curl`, `gcc`, and LLVM/clang with `libclang` (`dnf install clang-devel`). Then:

```bash
./scripts/10-install-toolchain.sh
./scripts/20-fetch-source.sh
./scripts/30-build.sh
./scripts/40-install.sh
```

`cmux` is on `PATH` afterwards (installs into `~/.local/bin`, which is on the default Fedora PATH).

With `gtk4-devel` and `vte291-gtk4-devel` installed (`sudo dnf install gtk4-devel vte291-gtk4-devel`), `30-build.sh` also builds the GTK app and `40-install.sh` installs it as `cmux-gtk`, with a **cmux** entry in your application launcher that opens the `main` session. Without those packages the GTK app is skipped and everything else installs as usual.

## Run

```bash
cmux server start --session agents   # durable headless owner (blocks: run under systemd or a separate terminal)
cmux attach --session agents         # attach the TUI (detach: prefix d -> Ctrl-b d)
cmux workspace create --name api
cmux terminal list
cmux-tui --help
```

### Browser tabs

cmux browser tabs (prefix `B` in the TUI) are pages that a *browser provider* lends to the session. Upstream's provider, cmux-browser, is a patched Chromium that isn't available on Linux, so without one, browser tabs stay at "starting". This repo builds its own, `cmux-browser-provider`, and `40-install.sh` installs it:

```bash
cmux-browser-provider --session agents   # keep running alongside the session (Ctrl-C to stop)
```

It needs a Chromium-based browser, but only when a browser tab exists: Chromium (`sudo dnf install chromium`), Brave or Edge, with Google Chrome as a last resort, or whatever `CMUX_LIN_BROWSER` names. It runs the browser headless with its own profile under `~/.local/share/cmux-lin/browsers/`, and the browser always exits with the provider. Terminals don't need it. The GTK prototype runs the same provider built in. Details are in [gui/cmux-gtk/README.md](gui/cmux-gtk/README.md#browser-tabs).

Upstream docs for everything the multiplexer can do live in `build/cmux/cmux-tui/docs/` and `build/cmux/cmux-tui/spec/` after fetching.

## Linux fixes

The upstream source builds unmodified; the three things that break a naive build are environmental, so `scripts/30-build.sh` handles them:

1. **Zig must be on `PATH`, not just `ZIG`.** `crates/ghostty-vt-sys/build.rs` uses `$ZIG` for the main `zig build` invocation, but Ghostty's `build.zig` shells out to `zig env` via `PATH`. Symptom: `the following command failed with FileNotFound: zig env`.
2. **Stale `LD_LIBRARY_PATH` breaks bindgen.** libclang must be loadable; anything prepending an old `libstdc++` (VS Code extension bundles are a known offender — they ship `GLIBCXX` versions older than the system LLVM) makes bindgen panic with `GLIBCXX_3.4.xx not found`. The build script runs the build with `LD_LIBRARY_PATH` unset.
3. **bindgen needs gcc's companion headers.** Clang chokes on `/usr/include/limits.h`'s `include_next` unless gcc's include dir is passed explicitly: `BINDGEN_EXTRA_CLANG_ARGS="-I$(gcc -print-libgcc-file-name | xargs dirname)/include"`.

## Theme

The multiplexer chrome (sidebar, tabs, pane borders, status bar, notification rings) is themed by `assets/cmux-tui.json` — a Catppuccin Mocha scheme with the original app's blue accents (active borders/rings `#89b4fa`, rounded pane corners, pill tabs, dimmed inactive panes). Color values come from the official Catppuccin palette.

### What a theme controls

Each theme file is `{"theme": {...}, "ghostty": {...}}`:

- `theme` — cmux-owned chrome: sidebar, tabs, pane borders, status bar, notification accents.
- `ghostty` — pane content: default background, foreground, cursor, selection, and the 16-color ANSI palette. cmux-tui reads `~/.config/ghostty/config` as application defaults for every hosted terminal, so `cmux-theme` maintains a managed block (between `# begin/end cmux-theme managed block` markers) in that file; anything else you put in it is preserved. If you also use the Ghostty terminal emulator, be aware this file colors it too.

Layering follows the "pane one notch above the app" rule: `status_bg` (and inactive tab chips) use the palette's base layer and the pane background uses its `surfaceContainer` equivalent — one elevation step lighter. Pane-content colors apply to terminals created after the switch; panes already running keep their paint until redrawn (apps that explicitly set colors are unaffected either way).

### scrollwerk themes

`scripts/convert-scrollwerk-themes.py` generates 32 more themes — dark and light variants of all 16 palettes from the scrollwerk Android launcher (`Palettes.kt`): Ayu, Catppuccin, Dracula, Eldritch, Gruvbox, Kanagawa, Noctalia, Nord, Rosé Pine, Tokyo-Night, Everforest, Solarized, One, Monokai Pro, Synthwave '84, Oxocarbon — named `sw-<palette>-<dark|light>`.

Their ANSI slots are mapped by Noctalia role, not by hue: red = `error`, yellow = `primary`, blue = `hover`, magenta = `tertiary`, green = `secondary`, grays = `surfaceContainer`/`outlineVariant`. A few schemes therefore shift hue in content (e.g. Dracula "green" prints pink) because that is what the source palette defines.

### Theme selector

`scripts/40-install.sh` installs a `cmux-theme` CLI plus shipped themes into `~/.config/cmux/themes/`:

```bash
cmux-theme list              # installed themes, * marks the active one
cmux-theme set mocha-mauve   # switch (also: mocha-blue, mocha-contrast, latte-light, default)
cmux-theme cycle             # next theme alphabetically, with wrap-around
```

Switching swaps only the `theme` section of `~/.config/cmux/cmux-tui.json` — every other setting is preserved — and then live-reloads every running session, so an attached TUI re-themes on the spot without restarting.

From **inside the TUI**, `Ctrl-b T` runs the cycle command (defined in the config's `commands` section, run silently in the background). `cmux-theme install <file>` adds your own theme — a JSON file of the shape `assets/themes/*`.

Two things the config cannot control:

- **Pane content colors for themes without a `ghostty` section** (i.e. `default`) come from cmux's built-in terminal defaults, and ANSI palette slots follow the host terminal's own palette.
- **Pill tab caps use Nerd Font glyphs.** Use a Nerd Font (e.g. JetBrains Mono Nerd Font) in the terminal, or they render as boxes.

## Local patch

`patches/expose-chrome-theme-keys.patch` is applied on top of the pinned commit by `30-build.sh` (idempotent: skipped when already applied).

Upstream's theme schema exposes only some chrome surfaces. The internal `ChromeTheme` — context menus, prompts/dialogs, toasts, the status-bar's active pill segments, the tab-bar backdrop, sidebar chrome — is hardcoded to xterm grays 235-246, which reads as "Windows 95 gray". The patch adds 17 theme keys (`menu_bg/fg/border`, `menu_selected_bg/fg`, `prompt_bg/fg/border`, `toast_bg/fg`, `status_active_bg/fg`, `status_dim_fg`, `tab_bar_bg`, `sidebar_selected_bg`, `sidebar_border`, `sidebar_dim_fg`), applies them onto the resolved `ChromeTheme` at startup, and re-derives chrome on config reload so `Ctrl-b T` re-themes every surface live. Themes carry the new keys; a theme with unknown keys stays valid on older binaries (unknown theme keys are ignored there, but the config schema rejects nothing).

The rest of `ChromeTheme` is exposed too, so no chrome surface stays on built-in grays: `prompt_title_fg`, `prompt_input_bg/fg`, `prompt_button_accent_fg`, `prompt_button_hover_bg`, `tab_fg`, `tab_active_fg`, `tab_active_unfocused_bg/fg`, `tab_plain_fg`, `tab_plain_active_fg`, `tab_plain_unfocused_fg`, `tab_control_hover_fg`, `omnibar_fg/sep_fg/dim_fg/edit_bg/edit_fg/hover_fg`, `browser_message_fg`, `scrollbar_thumb_fg`, `scrollbar_thumb_active_fg`, `foreign_viewport_bg/boundary_fg/hint_fg`. Themes don't need to set them. Any key a theme leaves out is derived from its core keys: accents from `border_active`, separators and borders from `border_inactive`, text from `sidebar_fg`/`sidebar_selected_fg`, dim text from `status_dim_fg`, backdrops from `tab_bar_bg`/`status_bg`. `border_active`/`border_inactive` also feed the chrome border colours. Status-bar error text uses `notification_error` instead of fixed red.

The patch also fixes pane ANSI colours. A theme's `ghostty` palette becomes the terminal's *default* palette, and upstream only paints palette slots that differ from the default (OSC 4 overrides) as RGB. Every other slot went out as a plain ANSI index, so the host terminal (kitty etc.) used its own palette and pane colours ignored the cmux theme. Slots the Ghostty config defines now always render as RGB.

## Pinning philosophy

`PINS.env` holds the exact `cmux` commit, the expected ghostty submodule commit, and the Zig version + sha256. Rebuilding a different upstream revision means changing that file, not the scripts. The source checkout is shallow (single-commit fetch of the pin), so the pins are the reproducibility contract.

## Credits

cmux-lin is an unofficial Linux build of other people's work. It isn't affiliated with or endorsed by the upstream projects.

- **[cmux](https://github.com/manaflow-ai/cmux)** by [manaflow-ai](https://github.com/manaflow-ai): the multiplexer itself. Everything `cmux-tui` does comes from upstream; this repo only pins, patches and builds it. Bugs found in this build should be reproduced here before being reported upstream.
- **[Ghostty](https://github.com/ghostty-org/ghostty)** by Mitchell Hashimoto and contributors: `libghostty-vt`, the terminal emulation core that cmux-tui renders every pane with.
- **[Zig](https://ziglang.org)** and the **[Rust](https://www.rust-lang.org)** toolchain, which build the above.
- **Colour palettes:** [Catppuccin](https://github.com/catppuccin/catppuccin) (the `mocha-*` and `latte-*` themes), plus Ayu, Dracula, Eldritch, Gruvbox, Kanagawa, Noctalia, Nord, Rosé Pine, Tokyo Night, Everforest, Solarized, One, Monokai Pro, Synthwave '84 and Oxocarbon, via the scrollwerk launcher's palette definitions (the `sw-*` themes). The colours belong to their respective authors.

## License

This repository is licensed under the GNU General Public License v3.0 or later; see [LICENSE](LICENSE).

- **cmux-tui** is GPL-3.0-or-later under upstream cmux's top-level license (a few unrelated upstream server directories are BUSL-1.1; none of them are built here). The binary you build is therefore GPL-3.0-or-later.
- **Ghostty** is MIT-licensed, which is compatible with the GPL build.
- **`patches/`** modifies upstream cmux-tui source, so the patches are GPL-3.0-or-later derivative works.
- **Build scripts, themes and docs** are this repo's own work, released under the same GPL-3.0-or-later terms.

## Desktop pairing

`cmux-theme pair <name> <desktop-theme>` pins one of the machine's desktop themes (`~/.config/waybar/themes/<name>`, applied by the local `waybar-theme.sh`) to a cmux theme. While a pairing exists, switching to that theme — via CLI, cycle, or `Ctrl-b T` — also re-applies the desktop theme, retinting waybar, the niri focus-ring, kitty, starship, and dunst in one step. `cmux-theme unpair <name>` removes it. Pairings are per-machine and stored in the theme file's optional `"desktop"` key, so nothing is coupled until you pair it. The desktop pipeline names its themes differently (v-prefixed series), so pairings are never auto-guessed.

## Quitting a session

A session server keeps running when you detach (`Ctrl-b d`), when the TUI closes, and even when its last workspace is closed. That is deliberate, so shells survive, but it means sessions pile up unless you stop them. `Ctrl-b Q` ends the session completely: it runs `cmux server stop` against the session it's pressed in (via the inherited `CMUX_TUI_SOCKET`), every shell in it ends, and the attached TUI exits. The saved layout is kept for the next start. `scripts/install-theme-picker.py` adds this `quit-session` command on every install, unless a command with that id already exists.

## In-TUI theme picker

Right-click the tab bar's `+` button: the menu lists every installed theme (plus `Default (upstream)`). Picking one runs `cmux-theme set <name>` through the config `commands` section — silently, no pane flash. `scripts/install-theme-picker.py` keeps the menu in sync with `~/.config/cmux/themes/` and runs on every `40-install.sh`; a user's own commands are preserved. The local patch also raises `MAX_USER_COMMANDS` from 32 to 256 so the full picker fits.

`Ctrl-b T` remains the keyboard cycle (perceptually ordered, see below).

## Live pane re-theming

Pane colors update without restarts, natively: a theme's `ghostty` section lands in `~/.config/ghostty/config`, cmux reads that file as its `terminal_defaults`, and the local patch pushes those defaults through the mux on every config reload — one broadcast repaints every live surface and becomes the default for panes created later. `cmux-theme set`/`cycle` and the `+` button's picker all trigger the same reload, so the whole of cmux — chrome and terminal panes — re-themes in one step.

One inherent limit, by ANSI semantics: content painted in explicit truecolor (fastfetch logos, prompt segments with fixed hex) keeps its literal colors, as in any terminal emulator.

Note: an earlier iteration pushed OSC color sequences into pane PTYs via `cmux terminal write`; that feeds the pane's input side, so shells echoed the payload as prompt garbage. Removed — the mux broadcast above is the correct channel.
