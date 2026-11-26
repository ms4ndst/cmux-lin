//! Agent attention: what needs the person, how urgently, and how to show it.
//!
//! Two server sources feed it: the agent roster (`list-agents` status) and
//! each tab's unread notification marker. Ranked most urgent first, as every
//! cmux client does: an agent waiting for input, an agent that finished a
//! turn nobody has looked at, an unread notification (by level), an agent
//! still working.

use std::path::PathBuf;

use gtk::prelude::*;
use serde_json::Value;

use crate::backend::{Agents, Tab};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attn {
    // Declared least to most urgent so `max()` picks the most urgent.
    Working,
    Info,
    Warning,
    Error,
    Done,
    Blocked,
}

impl Attn {
    /// Whether this asks the person to look (working is just activity).
    pub fn needs_you(self) -> bool {
        self != Attn::Working
    }

    fn glyph(self) -> &'static str {
        match self {
            Attn::Blocked | Attn::Info | Attn::Warning | Attn::Error => "●",
            Attn::Done => "✓",
            Attn::Working => "◐",
        }
    }

    fn css(self) -> &'static str {
        match self {
            Attn::Blocked => "att-blocked",
            Attn::Done => "att-done",
            Attn::Error => "att-error",
            Attn::Warning => "att-warning",
            Attn::Info => "att-info",
            Attn::Working => "att-working",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Attn::Blocked => "Agent is waiting for you",
            Attn::Done => "Agent finished",
            Attn::Error => "Unread error notification",
            Attn::Warning => "Unread warning",
            Attn::Info => "Unread notification",
            Attn::Working => "Agent working",
        }
    }
}

/// The most urgent attention state of one tab, if any.
pub fn of_tab(tab: &Tab, agents: &Agents) -> Option<Attn> {
    let agent = match agents.get(&tab.surface).map(|a| a.status.as_str()) {
        Some("blocked") => Some(Attn::Blocked),
        Some("done") => Some(Attn::Done),
        Some("working") => Some(Attn::Working),
        _ => None,
    };
    let marker = match tab.unread.as_deref() {
        Some("error") => Some(Attn::Error),
        Some("warning") => Some(Attn::Warning),
        Some(_) => Some(Attn::Info),
        None => None,
    };
    agent.max(marker)
}

/// A small coloured mark; `count` > 1 is shown next to it.
pub fn badge(attn: Attn, count: usize) -> gtk::Label {
    let text = if count > 1 { format!("{} {count}", attn.glyph()) } else { attn.glyph().to_string() };
    let label = gtk::Label::new(Some(&text));
    label.add_css_class("att");
    label.add_css_class(attn.css());
    label.set_tooltip_text(Some(attn.describe()));
    label
}

/// Badge colours from the active cmux theme (`~/.config/cmux/cmux-tui.json`,
/// the file `cmux-theme` writes), so badges match the TUI's notification
/// accents. Falls back to Catppuccin Mocha.
pub fn css() -> String {
    let theme = theme_colors();
    let pick = |key: &str, fallback: &str| theme.get(key).cloned().unwrap_or_else(|| fallback.to_string());
    let error = pick("notification_error", "#f38ba8");
    let warning = pick("notification_warning", "#f9e2af");
    let info = pick("notification_info", "#89b4fa");
    let accent = pick("border_active", "#89b4fa");
    format!(
        ".att {{ font-weight: bold; margin: 0 4px 0 0; }}\n\
         .att-blocked, .att-error {{ color: {error}; }}\n\
         .att-warning {{ color: {warning}; }}\n\
         .att-info {{ color: {info}; }}\n\
         .att-done {{ color: {accent}; }}\n\
         .att-working {{ color: {accent}; opacity: 0.7; }}\n"
    )
}

fn theme_colors() -> std::collections::HashMap<String, String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let Some(text) = dir.and_then(|dir| std::fs::read_to_string(dir.join("cmux/cmux-tui.json")).ok()) else {
        return Default::default();
    };
    let Ok(config) = serde_json::from_str::<Value>(&text) else { return Default::default() };
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
