//! cmux-gtk: a prototype native GTK4 frontend for a cmux-tui session.
//!
//! The cmux-tui server stays the source of truth. This window lists the
//! session's workspaces and shows the selected workspace's active terminal
//! in a VTE widget, fed by the server's byte-mode attachment (a VT replay
//! followed by live output). Keystrokes and the widget's cell grid go back
//! over the same attachment, so the server reflows the terminal to fit.
//!
//! Usage: cmux-gtk [--session NAME]   (default: main; CMUX_TUI_SOCKET wins)
//!
//! Debug: CMUX_GTK_SNAPSHOT=/abs/path.png renders the window to a PNG every
//! 2 s; CMUX_GTK_DEBUG=1 logs size reports.

mod backend;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use backend::{AttachEvent, Attachment, Ctl, UiMsg, Workspace};
use gtk::prelude::*;
use gtk::{gdk, glib};
use serde_json::Value;
use vte4::prelude::*;

const APP_ID: &str = "dev.cmuxlin.CmuxGtk";

struct State {
    socket_path: PathBuf,
    ctl: mpsc::Sender<Ctl>,
    ui_tx: async_channel::Sender<UiMsg>,
    workspaces: Vec<Workspace>,
    /// Workspace currently shown, by id.
    selected: Option<u64>,
    /// The attachment for the shown terminal, and its surface id.
    attachment: Option<Attachment>,
    surface: Option<u64>,
    /// Bumped on every switch; events from older attachments are dropped.
    generation: u64,
    /// Last grid sent with `resize-surface`.
    reported: Option<(u16, u16)>,
}

impl State {
    fn detach(&mut self) {
        // Dropping the attachment closes its connection, which also removes
        // this viewer's size report on the server.
        self.attachment = None;
        self.surface = None;
        self.reported = None;
    }
}

fn main() -> glib::ExitCode {
    let session = parse_session();
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, &session));
    // GTK must not see our own flags.
    app.run_with_args(&[std::env::args().next().unwrap_or_default()])
}

fn parse_session() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--session" {
            if let Some(name) = args.next() {
                return name;
            }
        } else if let Some(name) = arg.strip_prefix("--session=") {
            return name.to_string();
        }
    }
    "main".to_string()
}

fn build_ui(app: &gtk::Application, session: &str) {
    let config = match backend::config_for(session) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("cmux-gtk: {error}");
            app.quit();
            return;
        }
    };

    // --- widgets -------------------------------------------------------
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(format!("cmux — {session}"))
        .default_width(1200)
        .default_height(760)
        .build();

    let header = gtk::HeaderBar::new();
    let new_ws = gtk::Button::from_icon_name("list-add-symbolic");
    new_ws.set_tooltip_text(Some("New workspace"));
    header.pack_start(&new_ws);
    window.set_titlebar(Some(&header));

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    let placeholder = gtk::Label::new(Some("No workspaces.\nPress + to create one."));
    placeholder.set_justify(gtk::Justification::Center);
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(24);
    list.set_placeholder(Some(&placeholder));
    let sidebar = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(200)
        .build();

    let term = vte4::Terminal::new();
    term.set_hexpand(true);
    term.set_vexpand(true);
    term.set_scrollback_lines(10_000);
    term.set_font(Some(&gtk::pango::FontDescription::from_string("Monospace 11")));

    let status = gtk::Label::new(Some("Connecting…"));
    status.set_xalign(0.0);
    status.set_margin_start(8);
    status.set_margin_end(8);
    status.set_margin_top(2);
    status.set_margin_bottom(2);
    status.add_css_class("dim-label");

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&term);
    right.append(&status);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar));
    paned.set_end_child(Some(&right));
    paned.set_resize_start_child(false);
    paned.set_shrink_start_child(false);
    paned.set_position(220);
    window.set_child(Some(&paned));

    // --- backend -------------------------------------------------------
    let (ui_tx, ui_rx) = async_channel::unbounded::<UiMsg>();
    let (ctl_tx, ctl_rx) = mpsc::channel::<Ctl>();
    let socket_path = config.socket_path.clone();
    backend::spawn_control(config, ctl_rx, ui_tx.clone());

    let state = Rc::new(RefCell::new(State {
        socket_path,
        ctl: ctl_tx,
        ui_tx,
        workspaces: Vec::new(),
        selected: None,
        attachment: None,
        surface: None,
        generation: 0,
        reported: None,
    }));

    // Keystrokes and pastes: VTE has no PTY here, so `commit` hands us the
    // encoded input and we forward it over the attachment.
    term.connect_commit({
        let state = state.clone();
        move |_, text, _| {
            if let Some(attachment) = state.borrow().attachment.as_ref() {
                attachment.send(text.as_bytes());
            }
        }
    });

    new_ws.connect_clicked({
        let state = state.clone();
        move |_| {
            let _ = state.borrow().ctl.send(Ctl::NewWorkspace);
        }
    });

    list.connect_row_selected({
        let state = state.clone();
        let term = term.clone();
        let status = status.clone();
        move |_, row| {
            let Some(row) = row else { return };
            let index = row.index() as usize;
            let ws = state.borrow().workspaces.get(index).cloned();
            if let Some(ws) = ws {
                select(&state, &term, &status, &ws);
            }
        }
    });

    // Report the widget's cell grid when it changes. VTE resizes its own
    // grid to the allocation; we mirror it to the server.
    glib::timeout_add_local(Duration::from_millis(200), {
        let state = state.clone();
        let term = term.clone();
        move || {
            let mut st = state.borrow_mut();
            let size = grid(&term);
            if st.reported != Some(size) && size.0 > 1 && size.1 > 1 {
                if let Some(attachment) = st.attachment.as_ref() {
                    if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
                        eprintln!("cmux-gtk: resize {size:?} (was {:?})", st.reported);
                    }
                    attachment.resize(size.0, size.1);
                    st.reported = Some(size);
                }
            }
            glib::ControlFlow::Continue
        }
    });

    // --- UI message pump ----------------------------------------------
    glib::spawn_future_local({
        let state = state.clone();
        let term = term.clone();
        let list = list.clone();
        let status = status.clone();
        async move {
            while let Ok(msg) = ui_rx.recv().await {
                match msg {
                    UiMsg::Tree(workspaces) => on_tree(&state, &list, &term, &status, workspaces),
                    UiMsg::Status(text) => status.set_text(&text),
                    UiMsg::Attach { generation, event } => {
                        if generation != state.borrow().generation {
                            continue; // from a terminal we switched away from
                        }
                        match event {
                            AttachEvent::Replay { data, colors } => {
                                if let Some(colors) = colors {
                                    apply_colors(&term, &colors);
                                }
                                // VTE's reset keeps the visible screen, so
                                // clear screen and scrollback before a replay.
                                term.reset(true, true);
                                term.feed(b"\x1b[H\x1b[2J\x1b[3J");
                                term.feed(&data);
                            }
                            AttachEvent::Output(data) => term.feed(&data),
                            AttachEvent::Colors(colors) => apply_colors(&term, &colors),
                            AttachEvent::Ended(reason) => status.set_text(&reason),
                        }
                    }
                }
            }
        }
    });

    window.connect_close_request({
        let state = state.clone();
        move |_| {
            state.borrow_mut().detach();
            glib::Propagation::Proceed
        }
    });

    window.present();
    term.grab_focus();

    if let Some(path) = std::env::var_os("CMUX_GTK_SNAPSHOT") {
        let window = window.clone();
        glib::timeout_add_local(Duration::from_secs(2), move || {
            if let Err(error) = snapshot_png(&window, &path) {
                eprintln!("cmux-gtk: snapshot failed: {error}");
            }
            glib::ControlFlow::Continue
        });
    }
}

fn grid(term: &vte4::Terminal) -> (u16, u16) {
    let cols = term.column_count().clamp(0, i64::from(u16::MAX)) as u16;
    let rows = term.row_count().clamp(0, i64::from(u16::MAX)) as u16;
    (cols, rows)
}

/// Rebuild the sidebar when the tree changes, keeping the selection.
fn on_tree(
    state: &Rc<RefCell<State>>,
    list: &gtk::ListBox,
    term: &vte4::Terminal,
    status: &gtk::Label,
    workspaces: Vec<Workspace>,
) {
    let selected = state.borrow().selected;
    // Keep the current choice if it still exists; otherwise follow the
    // server's active workspace.
    let target = workspaces
        .iter()
        .position(|w| Some(w.id) == selected)
        .or_else(|| workspaces.iter().position(|w| w.active))
        .or(if workspaces.is_empty() { None } else { Some(0) });

    state.borrow_mut().workspaces = workspaces.clone();
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for ws in &workspaces {
        let name = if ws.name.is_empty() { format!("Workspace {}", ws.id) } else { ws.name.clone() };
        let title = gtk::Label::new(Some(&name));
        title.set_xalign(0.0);
        let subtitle = gtk::Label::new(Some(if ws.title.is_empty() { " " } else { &ws.title }));
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        subtitle.add_css_class("dim-label");
        subtitle.add_css_class("caption");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
        column.set_margin_top(6);
        column.set_margin_bottom(6);
        column.set_margin_start(6);
        column.append(&title);
        column.append(&subtitle);
        list.append(&column);
    }

    match target {
        Some(index) => {
            if let Some(row) = list.row_at_index(index as i32) {
                list.select_row(Some(&row));
            }
            // Re-selecting the same row is a no-op for GTK, so attach here if
            // the shown surface changed (e.g. the active tab moved).
            let ws = workspaces[index].clone();
            if state.borrow().surface != ws.surface {
                select(state, term, status, &ws);
            }
        }
        None => {
            let mut st = state.borrow_mut();
            st.detach();
            st.selected = None;
            term.reset(true, true);
            status.set_text("Session has no workspaces");
        }
    }
}

/// Show `ws`: attach to its active terminal, replacing any current one.
fn select(state: &Rc<RefCell<State>>, term: &vte4::Terminal, status: &gtk::Label, ws: &Workspace) {
    let mut st = state.borrow_mut();
    st.selected = Some(ws.id);
    if st.surface == ws.surface && st.attachment.is_some() {
        return;
    }
    st.detach();
    st.generation += 1;
    term.reset(true, true);
    let Some(surface) = ws.surface else {
        status.set_text("This workspace's active tab is not a terminal");
        return;
    };
    let size = grid(term);
    match Attachment::open(&st.socket_path, surface, (size.0.max(2), size.1.max(2)), st.generation, st.ui_tx.clone()) {
        Ok(attachment) => {
            st.attachment = Some(attachment);
            st.surface = Some(surface);
            st.reported = Some(size);
            let label = if ws.name.is_empty() { format!("workspace {}", ws.id) } else { ws.name.clone() };
            status.set_text(&format!("{label} · surface {surface}"));
        }
        Err(error) => status.set_text(&format!("attach failed: {error}")),
    }
}

/// Apply the server's terminal colors (from a replay or colors-changed
/// event) so the widget matches the active cmux theme.
///
/// The protocol carries fg/bg/cursor/selection, but its `palette` holds only
/// sparse per-terminal overrides (OSC 4). The theme's base palette is the one
/// the server itself loads from the Ghostty config, so read that same file;
/// it is re-read on every colors event, so theme switches follow live.
fn apply_colors(term: &vte4::Terminal, colors: &Value) {
    let hex = |key: &str| colors.get(key).and_then(Value::as_str).and_then(|s| gdk::RGBA::parse(s).ok());
    let overrides = colors.get("palette").and_then(Value::as_object);
    let base = ghostty_palette();
    let defaults = [
        "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
        "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    let palette: Vec<gdk::RGBA> = (0..16)
        .map(|i| {
            let from_override = overrides
                .and_then(|m| m.get(&i.to_string()))
                .and_then(Value::as_str)
                .and_then(|s| gdk::RGBA::parse(s).ok());
            from_override
                .or_else(|| base[i].clone())
                .unwrap_or_else(|| gdk::RGBA::parse(defaults[i]).expect("valid default color"))
        })
        .collect();
    let refs: Vec<&gdk::RGBA> = palette.iter().collect();
    term.set_colors(hex("fg").as_ref(), hex("bg").as_ref(), &refs);
    if let Some(cursor) = hex("cursor") {
        term.set_color_cursor(Some(&cursor));
    }
    if let Some(selection) = hex("selection_bg") {
        term.set_color_highlight(Some(&selection));
    }
    if let Some(selection) = hex("selection_fg") {
        term.set_color_highlight_foreground(Some(&selection));
    }
}

/// `palette = N=#rrggbb` entries 0-15 from the Ghostty config (last wins),
/// i.e. the block `cmux-theme` maintains.
fn ghostty_palette() -> Vec<Option<gdk::RGBA>> {
    let mut out = vec![None; 16];
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let Some(text) = dir.and_then(|dir| std::fs::read_to_string(dir.join("ghostty/config")).ok()) else {
        return out;
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() != "palette" {
            continue;
        }
        let Some((index, color)) = value.trim().split_once('=') else { continue };
        if let (Ok(index), Ok(rgba)) = (index.trim().parse::<usize>(), gdk::RGBA::parse(color.trim())) {
            if index < 16 {
                out[index] = Some(rgba);
            }
        }
    }
    out
}

fn snapshot_png(window: &gtk::ApplicationWindow, path: &std::ffi::OsStr) -> Result<(), String> {
    let (w, h) = (window.width(), window.height());
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(w), f64::from(h));
    let node = snapshot.to_node().ok_or("nothing rendered yet")?;
    let renderer = window.native().ok_or("no native")?.renderer().ok_or("no renderer")?;
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(path).map_err(|e| e.to_string())
}
