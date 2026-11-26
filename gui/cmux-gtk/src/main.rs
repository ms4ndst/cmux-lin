//! cmux-gtk: a prototype native GTK4 frontend for a cmux-tui session.
//!
//! The cmux-tui server stays the source of truth. This window lists the
//! session's workspaces and renders the selected workspace's active screen:
//! its split layout as nested `gtk::Paned`, and in every pane a tab strip
//! plus a VTE terminal for the pane's active tab. Each shown terminal has its
//! own byte-mode attachment (VT replay, then live output); keystrokes and the
//! widget's cell grid go back over that same attachment.
//!
//! Browser tabs show the page the session's browser provider runs (a URL
//! bar plus streamed frames, with mouse, wheel and keyboard forwarded). If no
//! provider is registered, the app runs one itself (the `browser-provider`
//! crate at the repo root).
//!
//! Agent attention (blocked / finished / unread notification / working) is
//! shown as badges on tabs and sidebar rows; notifications also go to the
//! desktop, and clicking one jumps to its tab.
//!
//! Layout actions (new tab, split, close, select tab, divider drags) are
//! sent to the server; the window only redraws from the tree the server
//! reports back, so it never diverges from the TUI.
//!
//! Usage: cmux-gtk [--session NAME]   (default: main; CMUX_TUI_SOCKET wins)
//!
//! Debug: CMUX_GTK_SNAPSHOT=/abs/path.png renders the window to a PNG every
//! 2 s; CMUX_GTK_DEBUG=1 logs size reports.

mod attention;
mod backend;
mod browser;

use cmux_browser_provider as provider;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use attention::Attn;
use backend::{Agents, AttachEvent, Attachment, Ctl, Layout, Pane, UiMsg, Workspace};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::{Value, json};
use vte4::prelude::*;

const APP_ID: &str = "dev.cmuxlin.CmuxGtk";

const CSS: &str = "
.pane-tabs { padding: 2px 4px; }
.pane-tabs button { padding: 2px 8px; min-height: 0; }
.pane-tabs .tab-active { font-weight: bold; box-shadow: inset 0 -2px @accent_color; }
.pane-tabs .tab-close { padding: 0 2px; min-width: 0; }
.pane.focused .pane-tabs { background: alpha(@accent_color, 0.12); }
.browser-bar { padding: 2px 4px; }
.browser-bar entry { min-height: 0; }
";

/// One pane on screen: its tab strip, its terminal, and the attachment for
/// the pane's active tab.
struct PaneView {
    root: gtk::Box,
    tabbar: gtk::Box,
    /// Shows `term` for a terminal tab, `web` for a browser tab.
    stack: gtk::Stack,
    term: vte4::Terminal,
    web: browser::WebView,
    /// Whether the current attachment is a browser tab.
    browser: bool,
    /// Whether that browser tab has been seen live on this attachment.
    browser_live: bool,
    /// The browser tab's stable id, for reading its selection.
    browser_tab: Option<String>,
    /// Intercepts the browser tab's file pickers (uploads).
    uploads: Option<provider::FileChooserWatch>,
    attachment: Option<Attachment>,
    surface: Option<u64>,
    /// Unique per attachment; routes attach events to this pane.
    generation: u64,
    reported: Option<(u16, u16)>,
    /// The pane state and tab attention the tab strip was last drawn from.
    drawn: Option<(Pane, Vec<Option<Attn>>)>,
}

impl PaneView {
    fn detach(&mut self) {
        // Dropping the attachment closes its connection, which also removes
        // this viewer's size report on the server.
        self.attachment = None;
        self.uploads = None;
        self.surface = None;
        self.reported = None;
    }
}

/// A built divider: the ratio it holds and a function that re-applies it.
struct SplitView {
    ratio: Rc<Cell<f64>>,
    apply: Rc<dyn Fn()>,
}

struct State {
    socket_path: PathBuf,
    ctl: mpsc::Sender<Ctl>,
    ui_tx: async_channel::Sender<UiMsg>,
    workspaces: Vec<Workspace>,
    /// Workspace currently shown, by id.
    selected: Option<u64>,
    /// Live pane views of the shown workspace, by pane id.
    panes: HashMap<u64, PaneView>,
    /// Workspace id + layout signature currently built.
    built: Option<String>,
    /// Pane whose terminal last had keyboard focus.
    focused: Option<u64>,
    /// Built dividers by split id, so ratio changes made elsewhere (the TUI)
    /// move them without a rebuild.
    splits: HashMap<u64, SplitView>,
    next_generation: u64,
    /// Agent status by surface, from the last poll.
    agents: Agents,
    /// What was last acknowledged per surface, so each finished turn or
    /// unread marker is acknowledged once.
    acked: HashMap<u64, String>,
    window: glib::WeakRef<gtk::ApplicationWindow>,
    app: glib::WeakRef<gtk::Application>,
    /// When a notification was last shown in the status line; the workspace
    /// summary doesn't overwrite it for a few seconds.
    noted_at: Option<std::time::Instant>,
    /// The built-in browser provider (idle if another one is registered).
    provider: Option<provider::ProviderHandle>,
    /// Why the built-in provider can't start a browser, if it can't.
    browser_problem: Option<String>,
}

impl State {
    fn send(&self, ctl: Ctl) {
        let _ = self.ctl.send(ctl);
    }

    fn workspace(&self) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| Some(w.id) == self.selected)
    }

    /// The pane keyboard shortcuts act on: the focused one, else the
    /// server's active pane.
    fn target_pane(&self) -> Option<u64> {
        self.focused
            .filter(|id| self.panes.contains_key(id))
            .or_else(|| self.workspace().and_then(|w| w.active_pane))
    }
}

/// Widgets the update functions need.
#[derive(Clone)]
struct Ui {
    list: gtk::ListBox,
    content: gtk::Box,
    status: gtk::Label,
    new_ws: gtk::Button,
}

type Shared = Rc<RefCell<State>>;

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

    let provider = gtk::CssProvider::new();
    provider.load_from_string(&format!("{CSS}{}", attention::css()));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

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

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_hexpand(true);
    content.set_vexpand(true);

    let status = gtk::Label::new(Some("Connecting…"));
    status.set_xalign(0.0);
    // Long messages must never widen the window.
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    status.set_hexpand(false);
    status.connect_label_notify(|label| label.set_tooltip_text(Some(&label.text())));
    status.set_margin_start(8);
    status.set_margin_end(8);
    status.set_margin_top(2);
    status.set_margin_bottom(2);
    status.add_css_class("dim-label");

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&content);
    right.append(&status);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar));
    paned.set_end_child(Some(&right));
    paned.set_resize_start_child(false);
    paned.set_shrink_start_child(false);
    paned.set_position(220);
    window.set_child(Some(&paned));

    let ui = Ui { list: list.clone(), content, status: status.clone(), new_ws: new_ws.clone() };
    set_live(&ui, false);

    // --- backend -------------------------------------------------------
    let (ui_tx, ui_rx) = async_channel::unbounded::<UiMsg>();
    let (ctl_tx, ctl_rx) = mpsc::channel::<Ctl>();
    let socket_path = config.socket_path.clone();
    backend::spawn_control(config, ctl_rx, ui_tx.clone());
    backend::spawn_events(socket_path.clone(), ctl_tx.clone(), ui_tx.clone());

    let state: Shared = Rc::new(RefCell::new(State {
        socket_path,
        ctl: ctl_tx,
        ui_tx,
        workspaces: Vec::new(),
        selected: None,
        panes: HashMap::new(),
        built: None,
        focused: None,
        splits: HashMap::new(),
        next_generation: 0,
        agents: Agents::new(),
        acked: HashMap::new(),
        window: window.downgrade(),
        app: app.downgrade(),
        noted_at: None,
        provider: None,
        browser_problem: None,
    }));

    // Browser tabs need a browser provider; run one unless disabled. It
    // stays idle when another provider (cmux-browser or a standalone
    // cmux-browser-provider) already owns the session's lease.
    if std::env::var_os("CMUX_GTK_NO_PROVIDER").is_none() {
        let ui_tx = state.borrow().ui_tx.clone();
        let handle = provider::start(state.borrow().socket_path.clone(), move |report| {
            let message = match report {
                provider::Report::Info(line) => UiMsg::Status(line),
                provider::Report::Problem(problem) => UiMsg::BrowserProblem(Some(problem)),
                provider::Report::Resolved => UiMsg::BrowserProblem(None),
            };
            let _ = ui_tx.send_blocking(message);
        });
        state.borrow_mut().provider = Some(handle);
    }

    new_ws.connect_clicked({
        let state = state.clone();
        move |_| state.borrow().send(Ctl::NewWorkspace)
    });

    list.connect_row_selected({
        let state = state.clone();
        let ui = ui.clone();
        move |_, row| {
            let Some(row) = row else { return };
            let id = state.borrow().workspaces.get(row.index() as usize).map(|w| w.id);
            if id.is_some() && id != state.borrow().selected {
                state.borrow_mut().selected = id;
                render(&state, &ui);
            }
        }
    });

    install_shortcuts(&window, &state);

    // Report every shown terminal's cell grid when it changes. VTE resizes
    // its own grid to the allocation; we mirror it to the server.
    glib::timeout_add_local(Duration::from_millis(200), {
        let state = state.clone();
        move || {
            let debug = std::env::var_os("CMUX_GTK_DEBUG").is_some();
            for (id, view) in state.borrow_mut().panes.iter_mut() {
                let size = if view.browser { view.web.grid(cell_pixels(&view.term)) } else { grid(&view.term) };
                if view.reported == Some(size) || size.0 < 2 || size.1 < 2 {
                    continue;
                }
                if let Some(attachment) = view.attachment.as_ref() {
                    if debug {
                        eprintln!("cmux-gtk: pane {id} resize {size:?} (was {:?})", view.reported);
                    }
                    attachment.resize(size.0, size.1);
                    view.reported = Some(size);
                }
            }
            glib::ControlFlow::Continue
        }
    });

    // --- UI message pump ----------------------------------------------
    glib::spawn_future_local({
        let state = state.clone();
        let ui = ui.clone();
        async move {
            while let Ok(msg) = ui_rx.recv().await {
                match msg {
                    UiMsg::Connected => set_live(&ui, true),
                    UiMsg::Copied { generation, result, cut } => on_copied(&state, &ui, generation, result, cut),
                    UiMsg::FileChooser { generation, node, multiple } => {
                        choose_files(&state, &ui, generation, node, multiple);
                    }
                    UiMsg::ContextMenu { generation, x, y, link } => {
                        let show = state
                            .borrow()
                            .panes
                            .values()
                            .find(|v| v.generation == generation)
                            .map(|v| v.web.menu_shower());
                        if let Some(show) = show {
                            show(x, y, link); // after the borrow ends
                        }
                    }
                    UiMsg::BrowserProblem(problem) => {
                        if let Some(problem) = &problem {
                            ui.status.set_text(problem);
                        }
                        state.borrow_mut().browser_problem = problem.clone();
                        // Browser tabs still starting show it in place.
                        for view in state.borrow().panes.values().filter(|v| v.browser && !v.browser_live) {
                            view.web.show_problem(problem.as_deref());
                        }
                    }
                    UiMsg::Disconnected(reason) => on_disconnected(&state, &ui, &reason),
                    UiMsg::Tree(workspaces, agents) => {
                        state.borrow_mut().agents = agents;
                        on_tree(&state, &ui, workspaces);
                    }
                    UiMsg::Notification { id, title, body, level, surface } => {
                        on_notification(&state, &ui, id, &title, &body, &level, surface);
                    }
                    UiMsg::Status(text) => ui.status.set_text(&text),
                    UiMsg::Attach { generation, event } => on_attach(&state, &ui, generation, event),
                }
            }
        }
    });

    window.connect_close_request({
        let state = state.clone();
        move |_| {
            let provider = {
                let mut st = state.borrow_mut();
                for view in st.panes.values_mut() {
                    view.detach();
                }
                st.provider.take()
            };
            drop(provider); // unregisters and closes its Chrome
            glib::Propagation::Proceed
        }
    });

    // Coming back to the window counts as seeing what it shows.
    window.connect_is_active_notify({
        let state = state.clone();
        move |_| ack_seen(&state)
    });

    // Clicking a desktop notification activates `app.show-surface`.
    let show_surface = gio::SimpleAction::new("show-surface", Some(glib::VariantTy::UINT64));
    show_surface.connect_activate({
        let state = state.clone();
        let ui = ui.clone();
        move |_, parameter| {
            if let Some(surface) = parameter.and_then(|p| p.get::<u64>()) {
                show_surface_tab(&state, &ui, surface);
            }
        }
    });
    app.add_action(&show_surface);

    // Copy / paste in the focused browser tab, also reachable as actions
    // (menus, D-Bus): `app.browser-copy`, `app.browser-paste`.
    let open_menu = gio::SimpleAction::new("browser-menu", None);
    open_menu.connect_activate({
        let state = state.clone();
        move |_, _| {
            // Resolve the view, release the borrow, then open (it calls back
            // into the state).
            let picture = {
                let st = state.borrow();
                st.target_pane().and_then(|id| st.panes.get(&id)).filter(|v| v.browser).map(|v| v.web.opener())
            };
            if let Some(open) = picture {
                open();
            }
        }
    });
    app.add_action(&open_menu);

    for (name, paste) in [("browser-copy", false), ("browser-paste", true)] {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate({
            let state = state.clone();
            move |_, _| {
                let view = {
                    let st = state.borrow();
                    st.target_pane().and_then(|id| st.panes.get(&id)).filter(|v| v.browser).map(|v| v.web.clip_handle())
                };
                if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
                    eprintln!("cmux-gtk: action {} on browser view: {}", if paste { "paste" } else { "copy" }, view.is_some());
                }
                if let Some((clip, paste_fn)) = view {
                    if paste { paste_fn() } else { clip(false) }
                }
            }
        });
        app.add_action(&action);
    }

    window.present();

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

/// Ctrl+Shift+T new tab, Ctrl+Shift+W close tab, Ctrl+Shift+E split right,
/// Ctrl+Shift+O split down, Ctrl+PageDown/PageUp next/previous tab. They act
/// on the focused pane and run in the capture phase, ahead of VTE.
fn install_shortcuts(window: &gtk::ApplicationWindow, state: &Shared) {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Global);
    let bind = |accel: &str, action: fn(&State, u64)| {
        let state = state.clone();
        let callback = gtk::CallbackAction::new(move |_, _| {
            let st = state.borrow();
            if let Some(pane) = st.target_pane() {
                action(&st, pane);
            }
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(accel), Some(callback)));
    };
    bind("<Control><Shift>t", |st, pane| st.send(new_tab(pane)));
    bind("<Control><Shift>e", |st, pane| st.send(split(pane, "right")));
    bind("<Control><Shift>o", |st, pane| st.send(split(pane, "down")));
    bind("<Control><Shift>b", |st, pane| st.send(new_browser_tab(pane)));
    bind("<Control><Shift>w", |st, pane| {
        if let Some(surface) = st.panes.get(&pane).and_then(|v| v.surface) {
            st.send(Ctl::request(json!({ "cmd": "close-surface", "surface": surface })));
        }
    });
    bind("<Control>Page_Down", |st, pane| {
        st.send(Ctl::request(json!({ "cmd": "select-tab", "pane": pane, "delta": 1 })));
    });
    bind("<Control>Page_Up", |st, pane| {
        st.send(Ctl::request(json!({ "cmd": "select-tab", "pane": pane, "delta": -1 })));
    });
    window.add_controller(controller);
}

fn new_tab(pane: u64) -> Ctl {
    Ctl::request(json!({ "cmd": "new-tab", "pane": pane }))
}

/// A browser tab starts blank; type a URL into its address bar.
fn new_browser_tab(pane: u64) -> Ctl {
    Ctl::request(json!({ "cmd": "new-browser-tab", "pane": pane, "url": "about:blank" }))
}

fn split(pane: u64, dir: &str) -> Ctl {
    Ctl::request(json!({ "cmd": "split", "pane": pane, "dir": dir }))
}

fn grid(term: &vte4::Terminal) -> (u16, u16) {
    let cols = term.column_count().clamp(0, i64::from(u16::MAX)) as u16;
    let rows = term.row_count().clamp(0, i64::from(u16::MAX)) as u16;
    (cols, rows)
}

/// Grey the window out (and block input) while the session is unreachable.
fn set_live(ui: &Ui, live: bool) {
    ui.list.set_sensitive(live);
    ui.content.set_sensitive(live);
    // Insensitive terminals barely change appearance; dim them visibly.
    ui.content.set_opacity(if live { 1.0 } else { 0.45 });
    ui.new_ws.set_sensitive(live);
}

/// The session stopped answering (stopped, or restarting). Close every
/// attachment but keep the pane widgets: when the session is back, the next
/// tree re-attaches each pane (a fresh replay) and reuses the layout if it
/// is unchanged. A session that isn't running yet lands here too.
fn on_disconnected(state: &Shared, ui: &Ui, reason: &str) {
    set_live(ui, false);
    for view in state.borrow_mut().panes.values_mut() {
        view.detach();
    }
    if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
        eprintln!("cmux-gtk: disconnected: {reason}");
    }
    ui.status.set_text("Session not running — waiting for it to start…");
}

/// A new tree from the server: refresh the sidebar and the shown workspace.
fn on_tree(state: &Shared, ui: &Ui, workspaces: Vec<Workspace>) {
    let selected = state.borrow().selected;
    // Keep the current choice if it still exists; otherwise follow the
    // server's active workspace.
    let target = workspaces
        .iter()
        .position(|w| Some(w.id) == selected)
        .or_else(|| workspaces.iter().position(|w| w.active))
        .or(if workspaces.is_empty() { None } else { Some(0) });
    {
        let mut st = state.borrow_mut();
        st.selected = target.map(|i| workspaces[i].id);
        st.workspaces = workspaces.clone();
    }

    // Rebuild the sidebar rows. Selecting a row fires `row-selected`, whose
    // handler sees `selected` already set and does nothing.
    while let Some(child) = ui.list.first_child() {
        ui.list.remove(&child);
    }
    for ws in &workspaces {
        let row = sidebar_row(ws, &state.borrow().agents);
        ui.list.append(&row);
    }
    if let Some(row) = target.and_then(|i| ui.list.row_at_index(i as i32)) {
        ui.list.select_row(Some(&row));
    }
    render(state, ui);
}

fn sidebar_row(ws: &Workspace, agents: &Agents) -> gtk::Box {
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
    column.set_hexpand(true);

    // The workspace's most urgent state, and how many tabs need you.
    let states: Vec<Attn> =
        ws.panes.iter().flat_map(|p| &p.tabs).filter_map(|t| attention::of_tab(t, agents)).collect();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.append(&column);
    if let Some(top) = states.iter().copied().max() {
        let count = states.iter().filter(|a| a.needs_you()).count();
        let badge = attention::badge(top, count);
        badge.set_valign(gtk::Align::Center);
        badge.set_margin_end(6);
        row.append(&badge);
    }
    row
}

/// Bring the content area in line with the selected workspace: rebuild the
/// split containers if the layout's structure changed, then refresh every
/// pane's tab strip and attachment.
fn render(state: &Shared, ui: &Ui) {
    let Some(ws) = state.borrow().workspace().cloned() else {
        clear_content(state, ui);
        ui.status.set_text("Session has no workspaces");
        return;
    };
    let Some(layout) = ws.layout.clone() else {
        clear_content(state, ui);
        ui.status.set_text("This workspace has no panes");
        return;
    };

    let signature = format!("{}:{}", ws.id, layout.signature());
    let rebuilt = state.borrow().built.as_deref() != Some(&signature);
    if rebuilt {
        rebuild(state, ui, &layout);
        state.borrow_mut().built = Some(signature);
    }

    sync_ratios(state, &layout);
    for pane in &ws.panes {
        if state.borrow().panes.contains_key(&pane.id) {
            refresh_pane(state, pane);
        }
    }
    // After a rebuild, put keyboard focus in the server's active pane (or
    // keep it where it was, if that pane survived).
    if rebuilt {
        let target = state.borrow().target_pane();
        // Clone the widget out first: grabbing focus runs the focus handler
        // synchronously, and it needs to borrow the state mutably.
        let widget: Option<gtk::Widget> = target.and_then(|id| {
            let st = state.borrow();
            st.panes.get(&id).map(|v| if v.browser { v.web.focus_widget() } else { v.term.clone().upcast() })
        });
        if let Some(widget) = widget {
            widget.grab_focus();
        }
    }
    update_focus_marks(state);
    let noted = state.borrow().noted_at.is_some_and(|at| at.elapsed() < Duration::from_secs(6));
    if noted {
        ack_seen(state);
        return;
    }
    let name = if ws.name.is_empty() { format!("workspace {}", ws.id) } else { ws.name.clone() };
    let panes = ws.panes.len();
    let tabs: usize = ws.panes.iter().map(|p| p.tabs.len()).sum();
    ui.status.set_text(&format!(
        "{name} · {panes} pane{} · {tabs} tab{}",
        if panes == 1 { "" } else { "s" },
        if tabs == 1 { "" } else { "s" }
    ));
    ack_seen(state);
}

fn clear_content(state: &Shared, ui: &Ui) {
    if let Some(child) = ui.content.first_child() {
        dismantle(&child);
        ui.content.remove(&child);
    }
    let mut st = state.borrow_mut();
    st.panes.clear(); // drops attachments
    st.splits.clear();
    st.built = None;
}

/// Rebuild the split tree, reusing existing pane views (and so their
/// terminals and attachments) by pane id.
fn rebuild(state: &Shared, ui: &Ui, layout: &Layout) {
    if let Some(child) = ui.content.first_child() {
        dismantle(&child);
        ui.content.remove(&child);
    }
    let wanted = layout.panes();
    {
        let mut st = state.borrow_mut();
        st.splits.clear();
        // A different workspace shares no panes; drop views that left.
        st.panes.retain(|id, _| wanted.contains(id));
    }
    for id in &wanted {
        if !state.borrow().panes.contains_key(id) {
            let view = new_pane_view(state, *id);
            state.borrow_mut().panes.insert(*id, view);
        }
    }
    let widget = build_layout(state, layout);
    ui.content.append(&widget);
}

/// Detach pane roots from the old `Paned` tree so they can be re-parented.
fn dismantle(widget: &gtk::Widget) {
    if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        let start = paned.start_child();
        let end = paned.end_child();
        paned.set_start_child(None::<&gtk::Widget>);
        paned.set_end_child(None::<&gtk::Widget>);
        for child in [start, end].into_iter().flatten() {
            dismantle(&child);
        }
    }
}

fn build_layout(state: &Shared, layout: &Layout) -> gtk::Widget {
    match layout {
        Layout::Leaf(id) => state.borrow().panes[id].root.clone().upcast(),
        Layout::Split { id, right, ratio, a, b } => {
            let orientation = if *right { gtk::Orientation::Horizontal } else { gtk::Orientation::Vertical };
            let paned = gtk::Paned::new(orientation);
            paned.set_wide_handle(true);
            paned.set_start_child(Some(&build_layout(state, a)));
            paned.set_end_child(Some(&build_layout(state, b)));
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
            watch_ratio(state, &paned, *id, *ratio);
            paned.upcast()
        }
    }
}

/// Keep the divider at a ratio rather than a pixel offset: re-apply it
/// whenever the paned's size changes, and treat other position changes as
/// user drags that set a new ratio, reported with `set-split-ratio`
/// (debounced). The ratio is of the paned's full extent, as in the TUI;
/// `max-position` is not that (it subtracts the second child's minimum).
fn watch_ratio(state: &Shared, paned: &gtk::Paned, split: Option<u64>, ratio: f64) {
    let extent = |paned: &gtk::Paned| match paned.orientation() {
        gtk::Orientation::Horizontal => paned.width(),
        _ => paned.height(),
    };
    let ratio = Rc::new(Cell::new(ratio));
    let applying = Rc::new(Cell::new(false));
    let apply: Rc<dyn Fn()> = Rc::new({
        let (ratio, applying, paned) = (ratio.clone(), applying.clone(), paned.downgrade());
        move || {
            let Some(paned) = paned.upgrade() else { return };
            let total = extent(&paned);
            if total > 0 {
                let want = (ratio.get() * f64::from(total)).round() as i32;
                applying.set(true);
                paned.set_position(want.clamp(paned.min_position(), paned.max_position()));
                applying.set(false);
            }
        }
    });
    paned.connect_notify_local(Some("max-position"), {
        let apply = apply.clone();
        move |_, _| apply()
    });
    if let Some(split) = split {
        state.borrow_mut().splits.insert(split, SplitView { ratio: ratio.clone(), apply });
    }
    let ctl = state.borrow().ctl.clone();
    let pending: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
    paned.connect_notify_local(Some("position"), move |paned, _| {
        let total = extent(paned);
        if applying.get() || total <= 0 {
            return;
        }
        ratio.set(f64::from(paned.position()) / f64::from(total));
        let Some(split) = split else { return };
        if let Some(source) = pending.take() {
            source.remove();
        }
        let (ctl, pending_inner, value) = (ctl.clone(), pending.clone(), ratio.get());
        pending.set(Some(glib::timeout_add_local_once(Duration::from_millis(300), move || {
            pending_inner.set(None);
            let _ = ctl.send(Ctl::request(json!({ "cmd": "set-split-ratio", "split": split, "ratio": value })));
        })));
    });
}

/// Move dividers whose ratio the server changed (e.g. dragged in the TUI).
/// Our own drags round-trip to the same value, so they don't re-apply.
fn sync_ratios(state: &Shared, layout: &Layout) {
    if let Layout::Split { id: Some(id), ratio, a, b, .. } = layout {
        let apply = state.borrow().splits.get(id).and_then(|split| {
            ((split.ratio.get() - ratio).abs() > 0.01).then(|| {
                split.ratio.set(*ratio);
                split.apply.clone()
            })
        });
        if let Some(apply) = apply {
            apply();
        }
        sync_ratios(state, a);
        sync_ratios(state, b);
    } else if let Layout::Split { a, b, .. } = layout {
        sync_ratios(state, a);
        sync_ratios(state, b);
    }
}

fn new_pane_view(state: &Shared, id: u64) -> PaneView {
    let tabbar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    tabbar.add_css_class("pane-tabs");

    let term = vte4::Terminal::new();
    term.set_hexpand(true);
    term.set_vexpand(true);
    term.set_scrollback_lines(10_000);
    term.set_font(Some(&gtk::pango::FontDescription::from_string("Monospace 11")));

    // VTE has no PTY here: `commit` hands us encoded keystrokes and pastes,
    // which go to this pane's attachment.
    term.connect_commit({
        let state = state.clone();
        move |_, text, _| {
            if let Some(attachment) = state.borrow().panes.get(&id).and_then(|v| v.attachment.as_ref()) {
                attachment.send(text.as_bytes());
            }
        }
    });

    // Focusing a terminal makes its pane the shortcut target and the
    // server's active pane (so the TUI and new tabs follow).
    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter({
        let state = state.clone();
        move |_| {
            let changed = state.borrow().focused != Some(id);
            if changed {
                state.borrow_mut().focused = Some(id);
                state.borrow().send(Ctl::request(json!({ "cmd": "focus-pane", "pane": id })));
                update_focus_marks(&state);
            }
        }
    });
    term.add_controller(focus);

    // Browser input goes over this pane's current (browser) attachment.
    let send: browser::Send = Rc::new({
        let state = state.clone();
        move |cmd, fields| {
            if let Some(attachment) = state.borrow().panes.get(&id).and_then(|v| v.attachment.as_ref()) {
                attachment.browser(cmd, fields);
            }
        }
    });
    // Requests the browser view can't do alone. Copy and the context menu's
    // link lookup are DevTools round trips, so they run off the UI thread
    // and report back through the UI channel.
    let host: browser::Host = Rc::new({
        let state = state.clone();
        move |request| {
            let st = state.borrow();
            let Some(view) = st.panes.get(&id) else { return };
            let (socket, ui_tx, generation) = (st.socket_path.clone(), st.ui_tx.clone(), view.generation);
            let tab_id = view.browser_tab.clone();
            match request {
                browser::HostRequest::Copy { cut } => {
                    let Some(tab_id) = tab_id else { return };
                    std::thread::spawn(move || {
                        let result = provider::selected_text(&socket, &tab_id);
                        let _ = ui_tx.send_blocking(UiMsg::Copied { generation, result, cut });
                    });
                }
                browser::HostRequest::OpenInNewTab(url) => {
                    st.send(Ctl::request(json!({ "cmd": "new-browser-tab", "pane": id, "url": url })));
                }
                browser::HostRequest::ContextMenu { x, y, page_x, page_y } => {
                    std::thread::spawn(move || {
                        // A failed lookup still shows the menu, just without
                        // the link items.
                        let link = tab_id.and_then(|tab| provider::link_at(&socket, &tab, page_x, page_y).ok().flatten());
                        let _ = ui_tx.send_blocking(UiMsg::ContextMenu { generation, x, y, link });
                    });
                }
            }
        }
    });
    let web = browser::WebView::new(send, host);
    let web_focus = gtk::EventControllerFocus::new();
    web_focus.connect_enter({
        let state = state.clone();
        move |_| {
            if state.borrow().focused != Some(id) {
                state.borrow_mut().focused = Some(id);
                state.borrow().send(Ctl::request(json!({ "cmd": "focus-pane", "pane": id })));
                update_focus_marks(&state);
            }
        }
    });
    web.root.add_controller(web_focus);

    let stack = gtk::Stack::new();
    stack.add_named(&term, Some("term"));
    stack.add_named(&web.root, Some("web"));
    stack.set_visible_child_name("term");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("pane");
    root.append(&tabbar);
    root.append(&stack);

    PaneView {
        root,
        tabbar,
        stack,
        term,
        web,
        browser: false,
        browser_live: false,
        browser_tab: None,
        uploads: None,
        attachment: None,
        surface: None,
        generation: 0,
        reported: None,
        drawn: None,
    }
}

/// Redraw a pane's tab strip if its tabs changed, and attach to its active
/// tab if that changed.
fn refresh_pane(state: &Shared, pane: &Pane) {
    let marks: Vec<Option<Attn>> = {
        let st = state.borrow();
        pane.tabs.iter().map(|t| attention::of_tab(t, &st.agents)).collect()
    };
    let drawn = (pane.clone(), marks);
    let needs_tabbar = state.borrow().panes[&pane.id].drawn.as_ref() != Some(&drawn);
    if needs_tabbar {
        let tabbar = state.borrow().panes[&pane.id].tabbar.clone();
        draw_tabbar(state, &tabbar, pane, &drawn.1);
        state.borrow_mut().panes.get_mut(&pane.id).expect("pane view").drawn = Some(drawn);
    }

    let active = pane.active().cloned();
    let surface = active.as_ref().map(|tab| tab.surface);
    if state.borrow().panes[&pane.id].surface == surface && surface.is_some() {
        return;
    }
    let mut st = state.borrow_mut();
    st.next_generation += 1;
    let generation = st.next_generation;
    let socket_path = st.socket_path.clone();
    let ui_tx = st.ui_tx.clone();
    let view = st.panes.get_mut(&pane.id).expect("pane view");
    view.detach();
    view.generation = generation;
    view.browser_live = false;
    let Some(tab) = active else {
        view.term.reset(true, true);
        view.term.feed(b"\x1b[H\x1b[2J\x1b[3J");
        return;
    };
    view.browser = !tab.terminal;
    view.browser_tab = if view.browser { tab.tab_id.clone() } else { None };
    // A headless page can't show a file dialog: watch for its requests and
    // answer them from ours.
    view.uploads = view.browser_tab.as_ref().map(|tab_id| {
        let ui_tx = ui_tx.clone();
        provider::watch_file_choosers(&socket_path, tab_id, move |chooser| {
            let _ = ui_tx.send_blocking(UiMsg::FileChooser { generation, node: chooser.node, multiple: chooser.multiple });
        })
    });
    let (size, cells) = if view.browser {
        view.stack.set_visible_child_name("web");
        view.web.clear();
        let cells = cell_pixels(&view.term);
        let size = view.web.grid(cells);
        // Before the page view has a size of its own, estimate it from the
        // pane (minus the address bar) rather than a fixed 80x24.
        let size = if size.0 < 2 || size.1 < 2 {
            let (w, h) = (view.root.width(), view.root.height() - 2 * cells.1 - 8);
            if w > cells.0 * 2 && h > cells.1 * 2 { ((w / cells.0) as u16, (h / cells.1) as u16) } else { (80, 24) }
        } else {
            size
        };
        (size, Some((cells.0 as u16, cells.1 as u16)))
    } else {
        view.stack.set_visible_child_name("term");
        view.term.reset(true, true);
        view.term.feed(b"\x1b[H\x1b[2J\x1b[3J");
        let size = grid(&view.term);
        ((size.0.max(2), size.1.max(2)), None)
    };
    match Attachment::open(&socket_path, tab.surface, size, cells, generation, ui_tx) {
        Ok(attachment) => {
            view.attachment = Some(attachment);
            view.surface = Some(tab.surface);
            view.reported = Some(size);
        }
        Err(error) => {
            let message = format!("\r\n  attach failed: {error}\r\n");
            view.term.feed(message.as_bytes());
        }
    }
}

/// One terminal cell in pixels (from the pane's VTE font, which is set even
/// while a browser tab is showing); the server sizes browser pages in cells.
fn cell_pixels(term: &vte4::Terminal) -> (i32, i32) {
    let (w, h) = (term.char_width() as i32, term.char_height() as i32);
    if w > 0 && h > 0 { (w, h) } else { (9, 20) }
}

fn draw_tabbar(state: &Shared, tabbar: &gtk::Box, pane: &Pane, marks: &[Option<Attn>]) {
    while let Some(child) = tabbar.first_child() {
        tabbar.remove(&child);
    }
    let ctl = state.borrow().ctl.clone();
    let send = move |ctl_msg: Ctl| {
        let _ = ctl.send(ctl_msg);
    };
    for (index, tab) in pane.tabs.iter().enumerate() {
        let title = if tab.title.is_empty() { format!("Tab {}", index + 1) } else { tab.title.clone() };
        let label = gtk::Label::new(Some(&title));
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_max_width_chars(24);
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let mark = marks.get(index).copied().flatten();
        if let Some(attn) = mark {
            inner.append(&attention::badge(attn, 1));
        }
        inner.append(&label);
        let button = gtk::Button::new();
        button.set_child(Some(&inner));
        button.add_css_class("flat");
        let tooltip = match mark {
            Some(attn) => format!("{title}\n{}", attn.describe()),
            None => title.clone(),
        };
        button.set_tooltip_text(Some(&tooltip));
        if index == pane.active_tab {
            button.add_css_class("tab-active");
        }
        let (pane_id, surface) = (pane.id, tab.surface);
        button.connect_clicked({
            let send = send.clone();
            move |_| send(Ctl::request(json!({ "cmd": "select-tab", "pane": pane_id, "index": index })))
        });
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_tooltip_text(Some("Close tab (Ctrl+Shift+W)"));
        close.connect_clicked({
            let send = send.clone();
            move |_| send(Ctl::request(json!({ "cmd": "close-surface", "surface": surface })))
        });
        let group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        group.append(&button);
        group.append(&close);
        tabbar.append(&group);
    }

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    tabbar.append(&spacer);

    let pane_id = pane.id;
    let actions: [(&str, &str, fn(u64) -> Ctl); 4] = [
        ("+", "New tab (Ctrl+Shift+T)", new_tab),
        ("🌐", "New browser tab (Ctrl+Shift+B)", new_browser_tab),
        ("┃", "Split right (Ctrl+Shift+E)", |pane| split(pane, "right")),
        ("━", "Split down (Ctrl+Shift+O)", |pane| split(pane, "down")),
    ];
    for (text, tooltip, make) in actions {
        let button = gtk::Button::with_label(text);
        button.add_css_class("flat");
        button.set_tooltip_text(Some(tooltip));
        button.connect_clicked({
            let send = send.clone();
            move |_| send(make(pane_id))
        });
        tabbar.append(&button);
    }
}

/// Tint the focused pane's tab strip so it's clear where typing goes.
fn update_focus_marks(state: &Shared) {
    let st = state.borrow();
    let target = st.target_pane();
    for (id, view) in &st.panes {
        if Some(*id) == target {
            view.root.add_css_class("focused");
        } else {
            view.root.remove_css_class("focused");
        }
    }
}

/// Acknowledge what the person can see: the active tab of every shown pane,
/// while the window has focus. That clears its unread marker and turns a
/// finished agent turn (`done`) into `idle`, as the TUI does for the tab it
/// shows. Each state is acknowledged once.
fn ack_seen(state: &Shared) {
    let mut st = state.borrow_mut();
    if !st.window.upgrade().is_some_and(|w| w.is_active()) {
        return;
    }
    let Some(ws) = st.workspace().cloned() else { return };
    let mut to_ack = Vec::new();
    for pane in &ws.panes {
        let Some(tab) = pane.tabs.get(pane.active_tab) else { continue };
        if !st.panes.contains_key(&pane.id) {
            continue;
        }
        let done = st.agents.get(&tab.surface).is_some_and(|s| s == "done");
        if !done && tab.unread.is_none() {
            st.acked.remove(&tab.surface);
            continue;
        }
        let token = format!("{done}:{:?}", tab.unread);
        if st.acked.get(&tab.surface) != Some(&token) {
            st.acked.insert(tab.surface, token);
            to_ack.push(tab.surface);
        }
    }
    for surface in to_ack {
        if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
            eprintln!("cmux-gtk: ack surface {surface}");
        }
        st.send(Ctl::request(json!({ "cmd": "ack-tab-notifications", "surface": surface })));
    }
}

/// A notification arrived: note it in the status line, and post it to the
/// desktop unless the person is already looking at that terminal.
fn on_notification(state: &Shared, ui: &Ui, id: u64, title: &str, body: &str, level: &str, surface: Option<u64>) {
    let line = if body.is_empty() { title.to_string() } else { format!("{title} — {body}") };
    ui.status.set_text(&format!("🔔 {line}"));
    state.borrow_mut().noted_at = Some(std::time::Instant::now());

    let st = state.borrow();
    let window_active = st.window.upgrade().is_some_and(|w| w.is_active());
    let visible = surface.is_some_and(|s| st.panes.values().any(|v| v.surface == Some(s)));
    if window_active && visible {
        return;
    }
    let Some(app) = st.app.upgrade() else { return };
    let notification = gio::Notification::new(if title.is_empty() { "cmux" } else { title });
    if !body.is_empty() {
        notification.set_body(Some(body));
    }
    notification.set_priority(match level {
        "error" => gio::NotificationPriority::Urgent,
        "warning" => gio::NotificationPriority::High,
        _ => gio::NotificationPriority::Normal,
    });
    if let Some(surface) = surface {
        notification.set_default_action_and_target_value("app.show-surface", Some(&surface.to_variant()));
    }
    app.send_notification(Some(&format!("cmux-{id}")), &notification);
}

/// Jump to the tab showing `surface`: select its workspace here, make it the
/// active tab and pane on the server, and raise the window.
fn show_surface_tab(state: &Shared, ui: &Ui, surface: u64) {
    let found = state.borrow().workspaces.iter().find_map(|ws| {
        ws.panes.iter().find_map(|pane| {
            let index = pane.tabs.iter().position(|t| t.surface == surface)?;
            Some((ws.id, pane.id, index))
        })
    });
    let Some((ws_id, pane_id, index)) = found else { return };
    {
        let mut st = state.borrow_mut();
        st.selected = Some(ws_id);
        st.focused = Some(pane_id);
        st.send(Ctl::request(json!({ "cmd": "select-tab", "pane": pane_id, "index": index })));
        st.send(Ctl::request(json!({ "cmd": "focus-pane", "pane": pane_id })));
    }
    let position = state.borrow().workspaces.iter().position(|w| w.id == ws_id);
    if let Some(row) = position.and_then(|i| ui.list.row_at_index(i as i32)) {
        ui.list.select_row(Some(&row));
    }
    render(state, ui);
    if let Some(window) = state.borrow().window.upgrade() {
        window.present();
    }
}

/// A page asked for a file: show the desktop file dialog (one file or
/// several, as the page's input allows) and give the page what was picked.
/// Cancelling leaves the input as it was.
fn choose_files(state: &Shared, ui: &Ui, generation: u64, node: i64, multiple: bool) {
    if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
        eprintln!("cmux-gtk: page asks for {} (input node {node})", if multiple { "files" } else { "a file" });
    }
    let Some(window) = state.borrow().window.upgrade() else { return };
    let dialog = gtk::FileDialog::builder()
        .title(if multiple { "Choose files to upload" } else { "Choose a file to upload" })
        .modal(true)
        .build();
    let (state, status) = (state.clone(), ui.status.clone());
    glib::spawn_future_local(async move {
        let files: Vec<gio::File> = if multiple {
            match dialog.open_multiple_future(Some(&window)).await {
                Ok(list) => (0..list.n_items()).filter_map(|i| list.item(i).and_downcast::<gio::File>()).collect(),
                Err(_) => return, // cancelled
            }
        } else {
            match dialog.open_future(Some(&window)).await {
                Ok(file) => vec![file],
                Err(_) => return,
            }
        };
        let paths: Vec<String> =
            files.iter().filter_map(|f| f.path()).map(|p| p.to_string_lossy().into_owned()).collect();
        if paths.is_empty() {
            status.set_text("Only local files can be uploaded");
            return;
        }
        let count = paths.len();
        let st = state.borrow();
        match st.panes.values().find(|v| v.generation == generation).and_then(|v| v.uploads.as_ref()) {
            Some(uploads) => {
                uploads.set_files(node, paths);
                status.set_text(&format!("Uploading {count} file{}", if count == 1 { "" } else { "s" }));
            }
            None => status.set_text("The page changed before the files were chosen"),
        }
    });
}

/// A browser tab's selection arrived: put it on the desktop clipboard, and
/// for cut, delete it in the page now that it's safely copied.
fn on_copied(state: &Shared, ui: &Ui, generation: u64, result: Result<String, String>, cut: bool) {
    if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
        eprintln!("cmux-gtk: copy result {:?}", result.as_ref().map(|t| t.chars().count()));
    }
    match result {
        Ok(text) if text.is_empty() => ui.status.set_text("Nothing selected"),
        Ok(text) => {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
            let chars = text.chars().count();
            ui.status.set_text(&format!("{} {chars} character{}", if cut { "Cut" } else { "Copied" }, if chars == 1 { "" } else { "s" }));
            if cut {
                let st = state.borrow();
                let attachment = st.panes.values().find(|v| v.generation == generation).and_then(|v| v.attachment.as_ref());
                if let Some(attachment) = attachment {
                    attachment.browser("browser-key-press", json!({
                        "key": "Delete", "code": "Delete", "modifiers": 0, "windows_virtual_key_code": 46,
                    }));
                }
            }
        }
        Err(error) => ui.status.set_text(&format!("Copy failed: {error}")),
    }
}

fn on_attach(state: &Shared, ui: &Ui, generation: u64, event: AttachEvent) {
    let st = state.borrow();
    let Some((_, view)) = st.panes.iter().find(|(_, v)| v.generation == generation) else {
        return; // from an attachment that has since been replaced
    };
    let term = &view.term;
    match event {
        AttachEvent::BrowserState { url, status, error, frame } => {
            if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
                let size = frame.as_ref().map(|f| (f.width, f.height, f.pointer_seq));
                eprintln!("cmux-gtk: browser-state {status} {url} frame {size:?}");
            }
            view.web.show_state(&url, &status, error.as_deref(), st.browser_problem.as_deref());
            if let Some(frame) = frame {
                if let (Some(seq), Some(attachment)) = (view.web.show_frame(frame), view.attachment.as_ref()) {
                    attachment.browser("browser-frame-presented", json!({ "frame_seq": seq }));
                }
            }
            if status == "live" && !view.browser_live {
                // Sizes reported while the page was starting may not have
                // reached it, and a static page won't repaint by itself.
                // Report one row less now; the resize tick then reports the
                // exact grid, a real geometry change that renders a frame.
                if let (Some(attachment), Some((cols, rows))) = (view.attachment.as_ref(), view.reported) {
                    attachment.resize(cols, rows.saturating_sub(1).max(1));
                }
                drop(st);
                let mut st = state.borrow_mut();
                if let Some(view) = st.panes.values_mut().find(|v| v.generation == generation) {
                    view.browser_live = true;
                    view.reported = None;
                }
                return;
            }
        }
        AttachEvent::Frame(frame) => {
            if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
                eprintln!("cmux-gtk: frame {}x{} seq {:?}", frame.width, frame.height, frame.pointer_seq);
            }
            // Acknowledge each frame once it's on screen; only then may
            // pointer input be aimed at it.
            if let (Some(seq), Some(attachment)) = (view.web.show_frame(frame), view.attachment.as_ref()) {
                attachment.browser("browser-frame-presented", json!({ "frame_seq": seq }));
            }
        }
        AttachEvent::Replay { data, colors } => {
            if let Some(colors) = colors {
                apply_colors(term, &colors);
            }
            // VTE's reset keeps the visible screen, so clear screen and
            // scrollback before a replay.
            term.reset(true, true);
            term.feed(b"\x1b[H\x1b[2J\x1b[3J");
            term.feed(&data);
        }
        AttachEvent::Output(data) => term.feed(&data),
        AttachEvent::Colors(colors) => apply_colors(term, &colors),
        AttachEvent::Ended(reason) => ui.status.set_text(&reason),
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
