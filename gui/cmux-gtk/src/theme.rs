//! The window's own colours, from the active cmux theme.
//!
//! Terminals already follow the theme (the server sends their colours). The
//! window's chrome (header, sidebar, tab strips, menus) would otherwise keep
//! the desktop's GTK theme and clash after a theme switch, so it's styled
//! from the same theme keys the TUI uses for its chrome: `status_bg` /
//! `status_fg` for the base layer, `sidebar_active_bg` / `sidebar_selected_fg`
//! for the selected row, `tab_bar_bg` / `tab_active_bg` for tab strips,
//! `menu_*` for popups, `border_active` as the accent, `border_inactive` for
//! lines. Pane backgrounds use the terminal background from the Ghostty
//! colour block, so there's no grey band below a terminal's last row.
//!
//! Missing keys fall back to the GTK theme's own colours (the rule is left
//! out), so the default upstream theme still looks like the desktop.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

/// The cmux config file `cmux-theme` writes (watched for theme changes).
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("cmux/cmux-tui.json"))
}

/// The active theme's colour keys (`#rrggbb` values only).
pub fn colors() -> HashMap<String, String> {
    let Some(text) = config_path().and_then(|path| std::fs::read_to_string(path).ok()) else {
        return HashMap::new();
    };
    let Ok(config) = serde_json::from_str::<Value>(&text) else { return HashMap::new() };
    config
        .get("theme")
        .and_then(Value::as_object)
        .map(|theme| {
            theme
                .iter()
                .filter_map(|(key, value)| {
                    let color = value.as_str()?;
                    color.starts_with('#').then(|| (key.clone(), color.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `background = #rrggbb` from the Ghostty config (the terminals' default
/// background, which `cmux-theme` keeps in step with the theme).
fn terminal_background() -> Option<String> {
    let text = std::fs::read_to_string(config_dir()?.join("ghostty/config")).ok()?;
    text.lines().rev().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "background").then(|| value.trim().to_string()).filter(|v| v.starts_with('#'))
    })
}

/// CSS for the window chrome. Each rule is emitted only when its colours
/// exist in the theme.
pub fn chrome_css() -> String {
    let c = colors();
    let get = |keys: &[&str]| keys.iter().find_map(|k| c.get(*k).cloned());
    let base = get(&["status_bg", "tab_bar_bg"]);
    let fg = get(&["status_fg", "sidebar_fg"]);
    let strong = get(&["sidebar_selected_fg", "menu_fg", "status_active_fg"]);
    let dim = get(&["status_dim_fg", "sidebar_dim_fg"]);
    let selected = get(&["sidebar_active_bg", "sidebar_selected_bg", "tab_active_bg"]);
    let strip = get(&["tab_bar_bg", "status_bg"]);
    let active_tab = get(&["tab_active_bg", "sidebar_active_bg"]);
    let accent = get(&["border_active"]);
    let line = get(&["border_inactive", "sidebar_border"]);
    let menu_bg = get(&["menu_bg", "prompt_bg"]);
    let menu_fg = get(&["menu_fg", "prompt_fg"]);
    let menu_hover = get(&["menu_selected_bg"]);
    let menu_hover_fg = get(&["menu_selected_fg"]);
    let input_bg = get(&["prompt_input_bg", "menu_bg"]);
    let pane_bg = terminal_background();

    let mut css = String::new();
    let mut rule = |selector: &str, decls: Vec<(&str, &Option<String>)>| {
        let body: Vec<String> =
            decls.into_iter().filter_map(|(prop, value)| value.as_ref().map(|v| format!("{prop}: {v};"))).collect();
        if !body.is_empty() {
            css.push_str(&format!("{selector} {{ {} }}\n", body.join(" ")));
        }
    };
    let none = Some("none".to_string());
    let transparent = Some("transparent".to_string());

    // Base layer: window, header bar, sidebar, status line.
    rule("window.cmux, window.cmux headerbar, .cmux-sidebar", vec![("background-color", &base), ("color", &fg)]);
    rule("window.cmux headerbar", vec![("box-shadow", &none), ("border-bottom-color", &line)]);
    rule("window.cmux headerbar button, window.cmux headerbar .title", vec![("color", &strong)]);
    rule(
        ".cmux-sidebar list, .cmux-sidebar scrolledwindow, .cmux-sidebar viewport",
        vec![("background-color", &transparent), ("color", &fg)],
    );
    rule(".cmux-sidebar list row:selected", vec![("background-color", &selected), ("color", &strong)]);
    rule(".cmux-sidebar list row:hover", vec![("background-color", &selected)]);
    rule(".cmux-sidebar stackswitcher button", vec![("color", &fg), ("background-color", &transparent)]);
    rule(".cmux-sidebar stackswitcher button:checked", vec![("color", &strong), ("background-color", &selected)]);
    rule(".cmux-sidebar entry, .cmux-sidebar searchentry", vec![("background-color", &input_bg), ("color", &strong)]);
    rule("window.cmux .dim-label", vec![("color", &dim)]);
    rule(".cmux-status", vec![("background-color", &base), ("color", &dim)]);

    // Tab strips (pane tabs, screens, browser address bar).
    rule(".pane-tabs, .screen-strip, .browser-bar", vec![("background-color", &strip), ("color", &fg)]);
    rule(
        ".pane-tabs button, .screen-strip button, .browser-bar button",
        vec![("color", &fg), ("background-color", &transparent)],
    );
    rule(".pane-tabs .tab-active, .screen-strip .tab-active", vec![("color", &strong), ("background-color", &active_tab)]);
    rule(".pane-tabs button:hover, .screen-strip button:hover", vec![("color", &strong)]);
    rule(".browser-bar entry", vec![("background-color", &input_bg), ("color", &strong)]);

    // Panes: the terminal background behind the grid; dividers in the line
    // colour.
    rule(".pane, .pane vte-terminal", vec![("background-color", &pane_bg)]);
    rule("window.cmux paned > separator, .column-handle", vec![("background-color", &line)]);

    // Popups: menus and popovers.
    rule("window.cmux popover > contents, popover.menu > contents", vec![("background-color", &menu_bg), ("color", &menu_fg)]);
    rule(".palette", vec![("background-color", &menu_bg), ("color", &menu_fg)]);
    rule(".palette list row:selected", vec![("background-color", &menu_hover), ("color", &menu_hover_fg)]);
    rule(".palette entry", vec![("background-color", &input_bg), ("color", &strong)]);
    rule(
        "popover.menu modelbutton:hover, popover.menu modelbutton:selected",
        vec![("background-color", &menu_hover), ("color", &menu_hover_fg)],
    );
    // Accent lines (after the rules above, so they win).
    if let Some(accent) = &accent {
        css.push_str(&format!(
            ".pane-tabs .tab-active, .screen-strip .tab-active {{ box-shadow: inset 0 -2px {accent}; }}\n\
             .pane.focused .pane-tabs {{ box-shadow: inset 0 2px {accent}; }}\n\
             .column-handle:hover {{ background-color: {accent}; }}\n\
             .pane.flash {{ box-shadow: inset 0 0 0 3px {accent}; }}\n"
        ));
    }
    css
}

/// The terminal colours the theme defines: the Ghostty colour block that
/// `cmux-theme` writes (and the server would load at start).
#[derive(Debug, Default, Clone)]
pub struct TerminalColors {
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub cursor: Option<String>,
    pub selection_background: Option<String>,
    pub selection_foreground: Option<String>,
    pub palette: [Option<String>; 16],
}

/// Read the theme's terminal colours (last value of each key wins, as in
/// Ghostty).
pub fn terminal_colors() -> TerminalColors {
    let mut colors = TerminalColors::default();
    let Some(text) = config_dir().and_then(|dir| std::fs::read_to_string(dir.join("ghostty/config")).ok()) else {
        return colors;
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim().to_string();
        match key.trim() {
            "background" => colors.background = Some(value),
            "foreground" => colors.foreground = Some(value),
            "cursor-color" => colors.cursor = Some(value),
            "selection-background" => colors.selection_background = Some(value),
            "selection-foreground" => colors.selection_foreground = Some(value),
            "palette" => {
                if let Some((index, color)) = value.split_once('=') {
                    if let Ok(index) = index.trim().parse::<usize>() {
                        if index < 16 {
                            colors.palette[index] = Some(color.trim().to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    colors
}

/// The terminal config the server reads (watched with the cmux config, so a
/// theme switch recolours terminals even if only this file changed).
pub fn ghostty_config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("ghostty/config"))
}
