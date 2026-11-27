//! The Machines sidebar view: other machines to open a workspace on over
//! SSH, like the macOS app's `cmux ssh user@host`.
//!
//! It lists the hosts in `~/.ssh/config` (and the files it `Include`s),
//! the SSH machines in the cmux config's `machines`, and hosts connected to
//! recently; the field at the top takes any `user@host`. Picking one opens a
//! new workspace named after the host whose terminal runs `ssh <host>`, so
//! your SSH config (keys, jump hosts, ports) applies as usual.
//!
//! This is a plain SSH session in a local terminal. cmux-tui's own remote
//! machines (`cmux-tui remote`, a cmux daemon on the other side) aren't
//! driven from here.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::prelude::*;
use serde_json::Value;

/// What the view asks of its host: open a workspace on this SSH target.
pub type Host = Rc<dyn Fn(String)>;

#[derive(Clone)]
pub struct MachinesView {
    pub root: gtk::Box,
    list: gtk::ListBox,
    entry: gtk::SearchEntry,
    /// The target per row, in row order.
    targets: Rc<RefCell<Vec<String>>>,
}

impl MachinesView {
    pub fn new(host: Host) -> Self {
        let entry = gtk::SearchEntry::new();
        entry.set_placeholder_text(Some("user@host — Enter to connect"));
        entry.set_margin_start(4);
        entry.set_margin_end(4);
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_activate_on_single_click(true);
        let placeholder = gtk::Label::new(Some("No hosts in ~/.ssh/config.\nType user@host above."));
        placeholder.set_justify(gtk::Justification::Center);
        placeholder.add_css_class("dim-label");
        placeholder.set_margin_top(18);
        list.set_placeholder(Some(&placeholder));
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let hint = gtk::Label::new(Some("Opens a workspace running ssh"));
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");
        hint.set_xalign(0.0);
        hint.set_margin_start(6);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.append(&entry);
        root.append(&hint);
        root.append(&scroller);

        let view = Self { root, list, entry, targets: Rc::new(RefCell::new(Vec::new())) };
        view.entry.connect_activate({
            let (view, host) = (view.clone(), host.clone());
            move |entry| {
                let target = entry.text().trim().to_string();
                if valid_target(&target) {
                    entry.set_text("");
                    host(target);
                    view.reload();
                }
            }
        });
        view.entry.connect_search_changed({
            let view = view.clone();
            move |_| view.reload()
        });
        view.list.connect_row_activated({
            let (view, host) = (view.clone(), host.clone());
            move |_, row| {
                let target = view.targets.borrow().get(row.index() as usize).cloned();
                if let Some(target) = target {
                    host(target);
                    view.reload();
                }
            }
        });
        // Re-read the SSH config whenever the view is shown.
        view.root.connect_map({
            let view = view.clone();
            move |_| view.reload()
        });
        view
    }

    /// Redraw the host list (recent first, then the rest by name), filtered
    /// by what's typed.
    pub fn reload(&self) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let filter = self.entry.text().to_lowercase();
        let recent = recent();
        let mut rows: Vec<(String, &str)> = recent.iter().map(|h| (h.clone(), "recent")).collect();
        for (host, origin) in known_hosts() {
            if !rows.iter().any(|(h, _)| *h == host) {
                rows.push((host, origin));
            }
        }
        let mut targets = Vec::new();
        for (host, origin) in rows.into_iter().filter(|(h, _)| filter.is_empty() || h.to_lowercase().contains(&filter)) {
            let icon = gtk::Image::from_icon_name("network-server-symbolic");
            let name = gtk::Label::new(Some(&host));
            name.set_xalign(0.0);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            name.set_hexpand(true);
            let from = gtk::Label::new(Some(origin));
            from.add_css_class("dim-label");
            from.add_css_class("caption");
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_start(4);
            row.set_margin_top(3);
            row.set_margin_bottom(3);
            row.append(&icon);
            row.append(&name);
            row.append(&from);
            self.list.append(&row);
            targets.push(host);
        }
        *self.targets.borrow_mut() = targets;
    }
}

/// `host`, `user@host` or `user@host:port`-free SSH targets: no spaces or
/// leading dash (it's passed to ssh as one argument).
pub fn valid_target(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with('-')
        && target.chars().all(|c| c.is_ascii_alphanumeric() || "@._-:[]%".contains(c))
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Hosts from `~/.ssh/config` and the cmux config's SSH machines, with
/// where each came from. Patterns (`*`, `?`, `!`) are left out.
pub fn known_hosts() -> Vec<(String, &'static str)> {
    let mut hosts: BTreeSet<String> = BTreeSet::new();
    let mut seen_files = BTreeSet::new();
    read_ssh_config(&home().join(".ssh/config"), &mut hosts, &mut seen_files, 0);
    let mut out: Vec<(String, &'static str)> = hosts.into_iter().map(|h| (h, "ssh config")).collect();
    if let Some(path) = crate::theme::config_path() {
        if let Some(json) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            for machine in json.get("machines").and_then(Value::as_array).into_iter().flatten() {
                let host = machine.get("host").and_then(Value::as_str).unwrap_or("");
                let user = machine.get("user").and_then(Value::as_str);
                let target = match user {
                    Some(user) if !user.is_empty() => format!("{user}@{host}"),
                    _ => host.to_string(),
                };
                if valid_target(&target) && !out.iter().any(|(h, _)| *h == target) {
                    out.push((target, "cmux machines"));
                }
            }
        }
    }
    out
}

fn read_ssh_config(path: &Path, hosts: &mut BTreeSet<String>, seen: &mut BTreeSet<PathBuf>, depth: u32) {
    if depth > 8 || !seen.insert(path.to_path_buf()) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(keyword) = words.next() else { continue };
        // `Host=a b` is valid too.
        let (keyword, first) = match keyword.split_once('=') {
            Some((k, rest)) => (k.to_string(), Some(rest.to_string())),
            None => (keyword.to_string(), None),
        };
        let values: Vec<String> = first.into_iter().filter(|v| !v.is_empty()).chain(words.map(str::to_string)).collect();
        match keyword.to_lowercase().as_str() {
            "host" => {
                for value in values {
                    if !value.contains(['*', '?', '!']) && valid_target(&value) {
                        hosts.insert(value);
                    }
                }
            }
            "include" => {
                for value in values {
                    for file in include_files(&value) {
                        read_ssh_config(&file, hosts, seen, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The files an `Include` names: relative to `~/.ssh`, `~` expanded, and a
/// `*` in the last component matched against that folder.
fn include_files(pattern: &str) -> Vec<PathBuf> {
    let path = if let Some(rest) = pattern.strip_prefix("~/") {
        home().join(rest)
    } else if pattern.starts_with('/') {
        PathBuf::from(pattern)
    } else {
        home().join(".ssh").join(pattern)
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains('*') {
        return vec![path];
    }
    let (before, after) = name.split_once('*').unwrap_or((&name, ""));
    let Some(dir) = path.parent() else { return Vec::new() };
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(before) && n.ends_with(after))
        })
        .collect();
    files.sort();
    files
}

fn recent_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("cmux-lin/ssh-recent"))
}

/// Hosts connected to from here, newest first.
pub fn recent() -> Vec<String> {
    recent_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.lines().map(str::trim).filter(|l| valid_target(l)).map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn note_recent(target: &str) {
    let Some(path) = recent_path() else { return };
    let mut list = recent();
    list.retain(|h| h != target);
    list.insert(0, target.to_string());
    list.truncate(12);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, list.join("\n") + "\n");
}
