//! The files sidebar view, like the TUI's (`sidebar.view = "files"`): a
//! browser on the focused pane's working directory.
//!
//! It follows the focused pane's cwd until you navigate yourself (`~`
//! follows again). Double-click or Enter descends into a folder or opens a
//! file in `$EDITOR` in a new tab; `c` sends a quoted `cd` to the focused
//! pane; `o` opens `.html`/`.md` in a browser tab; `.` shows dotfiles;
//! Backspace or Left goes up. The host does anything that touches the
//! session, through [`Request`].
//!
//! Directories are read locally: the window and the session are on the same
//! machine (it talks to a local socket).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};

/// What the view asks of its host.
pub enum Request {
    /// `cd` the focused pane to this directory.
    Cd(PathBuf),
    /// Open this file in `$EDITOR` in a new tab.
    Edit(PathBuf),
    /// Open this file in a browser tab.
    Browse(PathBuf),
}

pub type Host = Rc<dyn Fn(Request)>;

/// Most entries listed (a huge directory stays responsive).
const MAX_ENTRIES: usize = 2000;

#[derive(Clone)]
pub struct FilesView {
    pub root: gtk::Box,
    path: gtk::Label,
    list: gtk::ListBox,
    filter: gtk::SearchEntry,
    follow: gtk::ToggleButton,
    hidden: gtk::ToggleButton,
    dir: Rc<RefCell<Option<PathBuf>>>,
    /// The entries shown, in row order.
    entries: Rc<RefCell<Vec<(PathBuf, bool)>>>,
}

impl FilesView {
    pub fn new(host: Host) -> Self {
        let up = gtk::Button::from_icon_name("go-up-symbolic");
        up.set_tooltip_text(Some("Parent folder (Backspace)"));
        let follow = gtk::ToggleButton::with_label("~");
        follow.set_tooltip_text(Some("Follow the focused pane's folder (~)"));
        follow.set_active(true);
        let hidden = gtk::ToggleButton::with_label(".");
        hidden.set_tooltip_text(Some("Show hidden files (.)"));
        let cd = gtk::Button::with_label("cd");
        cd.set_tooltip_text(Some("cd the focused pane here (c)"));
        for button in [up.upcast_ref::<gtk::Widget>(), follow.upcast_ref(), hidden.upcast_ref(), cd.upcast_ref()] {
            button.add_css_class("flat");
        }
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        bar.append(&up);
        bar.append(&follow);
        bar.append(&hidden);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        bar.append(&spacer);
        bar.append(&cd);

        let path = gtk::Label::new(None);
        path.set_xalign(0.0);
        path.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        path.add_css_class("dim-label");
        path.add_css_class("caption");
        path.set_margin_start(6);

        let filter = gtk::SearchEntry::new();
        filter.set_placeholder_text(Some("Filter"));
        filter.set_margin_start(4);
        filter.set_margin_end(4);

        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_activate_on_single_click(false);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 2);
        root.append(&bar);
        root.append(&path);
        root.append(&filter);
        root.append(&scroller);

        let view = Self {
            root,
            path,
            list,
            filter,
            follow,
            hidden,
            dir: Rc::new(RefCell::new(None)),
            entries: Rc::new(RefCell::new(Vec::new())),
        };

        up.connect_clicked({
            let view = view.clone();
            move |_| view.go_up()
        });
        view.hidden.connect_toggled({
            let view = view.clone();
            move |_| view.reload()
        });
        view.filter.connect_search_changed({
            let view = view.clone();
            move |_| view.reload()
        });
        cd.connect_clicked({
            let (view, host) = (view.clone(), host.clone());
            move |_| {
                if let Some(dir) = view.dir.borrow().clone() {
                    host(Request::Cd(dir));
                }
            }
        });
        view.list.connect_row_activated({
            let (view, host) = (view.clone(), host.clone());
            move |_, row| view.open(row.index() as usize, &host)
        });
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let view = view.clone();
            move |_, key, _, _| {
                let selected = view.selected();
                match key {
                    gdk::Key::BackSpace | gdk::Key::Left => view.go_up(),
                    gdk::Key::Right => {
                        if let Some(index) = selected.filter(|i| view.entries.borrow().get(*i).is_some_and(|e| e.1)) {
                            view.open(index, &host);
                        }
                    }
                    gdk::Key::c => {
                        let target = match selected.and_then(|i| view.entries.borrow().get(i).cloned()) {
                            Some((path, true)) => Some(path),
                            _ => view.dir.borrow().clone(),
                        };
                        if let Some(dir) = target {
                            host(Request::Cd(dir));
                        }
                    }
                    gdk::Key::o => {
                        if let Some((path, false)) = selected.and_then(|i| view.entries.borrow().get(i).cloned()) {
                            if is_document(&path) {
                                host(Request::Browse(path));
                            }
                        }
                    }
                    gdk::Key::period => view.hidden.set_active(!view.hidden.is_active()),
                    gdk::Key::asciitilde => view.follow.set_active(true),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        });
        view.list.add_controller(keys);
        view
    }

    /// Whether the view follows the focused pane's folder.
    pub fn following(&self) -> bool {
        self.follow.is_active()
    }

    /// The focused pane's folder changed: show it if following.
    pub fn follow_to(&self, dir: &Path) {
        if self.following() && self.dir.borrow().as_deref() != Some(dir) {
            self.show(dir.to_path_buf());
        }
    }

    fn show(&self, dir: PathBuf) {
        *self.dir.borrow_mut() = Some(dir);
        self.reload();
    }

    /// Navigating by hand stops following the pane.
    fn navigate(&self, dir: PathBuf) {
        self.follow.set_active(false);
        self.filter.set_text("");
        self.show(dir);
    }

    fn go_up(&self) {
        let parent = self.dir.borrow().as_ref().and_then(|d| d.parent().map(Path::to_path_buf));
        if let Some(parent) = parent {
            self.navigate(parent);
        }
    }

    fn selected(&self) -> Option<usize> {
        self.list.selected_row().map(|row| row.index() as usize)
    }

    fn open(&self, index: usize, host: &Host) {
        let Some((path, is_dir)) = self.entries.borrow().get(index).cloned() else { return };
        if is_dir {
            self.navigate(path);
        } else {
            host(Request::Edit(path));
        }
    }

    /// Re-read the folder and redraw the rows.
    fn reload(&self) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let Some(dir) = self.dir.borrow().clone() else {
            self.path.set_text("No folder (focus a terminal pane)");
            return;
        };
        self.path.set_text(&dir.display().to_string());
        self.path.set_tooltip_text(Some(&dir.display().to_string()));
        let show_hidden = self.hidden.is_active();
        let filter = self.filter.text().to_lowercase();
        let mut entries: Vec<(PathBuf, bool)> = match std::fs::read_dir(&dir) {
            Ok(read) => read
                .flatten()
                .filter(|e| show_hidden || !e.file_name().to_string_lossy().starts_with('.'))
                .filter(|e| filter.is_empty() || e.file_name().to_string_lossy().to_lowercase().contains(&filter))
                .map(|e| {
                    let path = e.path();
                    let is_dir = path.is_dir();
                    (path, is_dir)
                })
                .take(MAX_ENTRIES)
                .collect(),
            Err(error) => {
                self.path.set_text(&format!("{}: {error}", dir.display()));
                Vec::new()
            }
        };
        // Folders first, then by name (case-insensitive).
        entries.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| name(&a.0).to_lowercase().cmp(&name(&b.0).to_lowercase()))
        });
        for (path, is_dir) in &entries {
            let icon = gtk::Image::from_icon_name(if *is_dir { "folder-symbolic" } else { "text-x-generic-symbolic" });
            let label = gtk::Label::new(Some(&name(path)));
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_start(4);
            row.append(&icon);
            row.append(&label);
            self.list.append(&row);
        }
        *self.entries.borrow_mut() = entries;
    }
}

fn name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Files `o` opens in a browser tab, as in the TUI.
fn is_document(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
        Some("html" | "htm" | "md" | "markdown")
    )
}

/// Single-quote for a POSIX shell (`'` becomes `'\''`).
pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// `file://` URL with the path percent-encoded.
pub fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.display().to_string().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => url.push(byte as char),
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}
