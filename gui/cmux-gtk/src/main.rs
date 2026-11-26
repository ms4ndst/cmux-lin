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
mod files;
mod theme;

use cmux_browser_provider as provider;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use attention::Attn;
use backend::{Agents, AttachEvent, Attachment, Column, Ctl, Layout, Pane, UiMsg, Workspace};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::{Value, json};
use vte4::prelude::*;

/// Base application id; each session gets its own (`<base>.<session>`), so
/// every session has its own window, opening a session again raises its
/// window, and windows for different sessions never hand off to each other.
const APP_ID: &str = "dev.cmuxlin.CmuxGtk";

/// `dev.cmuxlin.CmuxGtk.<session>`, with the session name made a valid
/// application id element (letters, digits, `_`; not starting with a digit).
fn app_id_for(session: &str) -> String {
    let mut element: String =
        session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if element.is_empty() || element.starts_with(|c: char| c.is_ascii_digit()) {
        element.insert(0, 's');
    }
    format!("{APP_ID}.{element}")
}

const CSS: &str = "
.pane-tabs { padding: 2px 4px; }
.column-handle { min-width: 4px; background: alpha(currentColor, 0.15); }
.column-handle:hover { background: @accent_color; }
.screen-strip { padding: 2px 6px; border-bottom: 1px solid alpha(currentColor, 0.15); }
.screen-strip button { padding: 2px 8px; min-height: 0; }
.screen-strip .tab-active { font-weight: bold; box-shadow: inset 0 -2px @accent_color; }
.screen-strip .tab-close { padding: 0 2px; min-width: 0; }
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
    /// The terminal's right-click menu (detached when the view goes away).
    term_menu: gtk::PopoverMenu,
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

impl Drop for PaneView {
    fn drop(&mut self) {
        self.term_menu.unparent();
    }
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

/// A built viewport-column row: the scroller (whose width is "the
/// viewport") and each column with its width fraction and a pane in it (the
/// handle `set-viewport-pane-width` takes).
struct ViewportView {
    scroller: gtk::ScrolledWindow,
    columns: Vec<ColumnView>,
}

struct ColumnView {
    id: u64,
    widget: gtk::Box,
    fraction: Rc<Cell<f64>>,
    dragging: Rc<Cell<bool>>,
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
    /// This window's own screen per workspace and tab per pane. Selection is
    /// client-local, as in upstream's design (the TUI keeps its own too); the
    /// tree's active flags are only shared defaults, used for first display
    /// and for things just created.
    screen_choice: HashMap<u64, u64>,
    tab_choice: HashMap<u64, u64>,
    /// Ids seen so far, to recognise newly created workspaces, screens, tabs.
    known: Option<(HashSet<u64>, HashSet<u64>, HashSet<u64>)>,
    /// The tree as the server sent it; `workspaces` is it with the choices
    /// above applied.
    raw: Vec<Workspace>,
    /// Widgets, for handlers that change the selection and redraw.
    ui: Option<Ui>,
    /// The window's style sheet (chrome and badge colours follow the cmux
    /// theme), and the watch that restyles when the theme changes elsewhere.
    css: Option<gtk::CssProvider>,
    _theme_watch: Option<gio::FileMonitor>,
    /// The viewport-column row when the shown screen uses columns.
    viewport: Option<ViewportView>,
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
    /// The agents queue under the workspace list, and its rows.
    agents_box: gtk::Box,
    agents_list: gtk::ListBox,
    /// Workspaces / Files.
    sidebar_stack: gtk::Stack,
    files: files::FilesView,
    /// The selected workspace's screens, above the panes.
    screens: gtk::Box,
    content: gtk::Box,
    status: gtk::Label,
    new_ws: gtk::Button,
}

type Shared = Rc<RefCell<State>>;

fn main() -> glib::ExitCode {
    let session = parse_session();
    let app = gtk::Application::builder().application_id(app_id_for(&session)).build();
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
    provider.load_from_string(&full_css());
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

    window.add_css_class("cmux");
    let header = gtk::HeaderBar::new();
    let new_ws = gtk::Button::from_icon_name("list-add-symbolic");
    new_ws.set_tooltip_text(Some("New workspace"));
    header.pack_start(&new_ws);
    // Main menu: theme, workspace actions, sidebar view.
    let main_menu = gtk::MenuButton::new();
    main_menu.set_icon_name("open-menu-symbolic");
    main_menu.set_tooltip_text(Some("Menu"));
    main_menu.set_menu_model(Some(&build_main_menu()));
    header.pack_end(&main_menu);
    window.set_titlebar(Some(&header));

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    // Fill the sidebar, so the space below the rows is a drop target too.
    list.set_vexpand(true);
    // Single click selects; double-click (row activation) renames.
    list.set_activate_on_single_click(false);
    let placeholder = gtk::Label::new(Some("No workspaces.\nPress + to create one."));
    placeholder.set_justify(gtk::Justification::Center);
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(24);
    list.set_placeholder(Some(&placeholder));
    let workspaces_scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    // Agents queue (as in the TUI): every agent across all workspaces, most
    // urgent first; shown only while agents exist.
    let agents_list = gtk::ListBox::new();
    agents_list.add_css_class("navigation-sidebar");
    agents_list.set_activate_on_single_click(true);
    let agents_heading = gtk::Label::new(Some("Agents"));
    agents_heading.set_xalign(0.0);
    agents_heading.add_css_class("heading");
    agents_heading.set_margin_start(8);
    agents_heading.set_margin_top(6);
    let agents_scroller = gtk::ScrolledWindow::builder()
        .child(&agents_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(320)
        .build();
    let agents_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    agents_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    agents_box.append(&agents_heading);
    agents_box.append(&agents_scroller);
    agents_box.set_visible(false);
    let workspaces_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    workspaces_page.append(&workspaces_scroller);
    workspaces_page.append(&agents_box);

    // Files view; its requests are wired up once the state exists.
    let files_host_slot: Rc<RefCell<Option<files::Host>>> = Rc::new(RefCell::new(None));
    let files_view = files::FilesView::new(Rc::new({
        let slot = files_host_slot.clone();
        move |request| {
            let host = slot.borrow().clone();
            if let Some(host) = host {
                host(request);
            }
        }
    }));

    let sidebar_stack = gtk::Stack::new();
    sidebar_stack.add_titled(&workspaces_page, Some("workspaces"), "Workspaces");
    sidebar_stack.add_titled(&files_view.root, Some("files"), "Files");
    sidebar_stack.set_vexpand(true);
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&sidebar_stack));
    switcher.set_margin_start(4);
    switcher.set_margin_end(4);
    switcher.set_margin_top(4);
    switcher.set_tooltip_text(Some("Workspaces / Files (Ctrl+Shift+F)"));
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 2);
    sidebar.add_css_class("cmux-sidebar");
    sidebar.set_width_request(200);
    sidebar.append(&switcher);
    sidebar.append(&sidebar_stack);

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
    status.add_css_class("cmux-status");

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let screens = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    screens.add_css_class("screen-strip");
    right.append(&screens);
    right.append(&content);
    right.append(&status);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar));
    paned.set_end_child(Some(&right));
    paned.set_resize_start_child(false);
    paned.set_shrink_start_child(false);
    paned.set_position(220);
    window.set_child(Some(&paned));

    let ui = Ui {
        list: list.clone(),
        agents_box,
        agents_list: agents_list.clone(),
        sidebar_stack: sidebar_stack.clone(),
        files: files_view.clone(),
        screens,
        content,
        status: status.clone(),
        new_ws: new_ws.clone(),
    };
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
        screen_choice: HashMap::new(),
        tab_choice: HashMap::new(),
        known: None,
        raw: Vec::new(),
        ui: None,
        viewport: None,
        css: Some(provider.clone()),
        _theme_watch: watch_theme(&provider),
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
    state.borrow_mut().ui = Some(ui.clone());

    // Files view requests act on the focused pane.
    *files_host_slot.borrow_mut() = Some(Rc::new({
        let state = state.clone();
        move |request| {
            let st = state.borrow();
            let Some(pane) = st.target_pane() else { return };
            match request {
                files::Request::Cd(dir) => {
                    let terminal = st.panes.get(&pane).filter(|v| !v.browser).and_then(|v| v.attachment.as_ref());
                    if let Some(attachment) = terminal {
                        attachment.send(format!("cd {}\r", files::shell_quote(&dir)).as_bytes());
                    }
                }
                files::Request::Edit(file) => {
                    let command = format!("${{EDITOR:-vi}} {}", files::shell_quote(&file));
                    let mut request = json!({ "cmd": "run", "command": command, "pane": pane });
                    if let Some(dir) = file.parent() {
                        request["cwd"] = json!(dir.display().to_string());
                    }
                    st.send(Ctl::request(request));
                }
                files::Request::Browse(file) => {
                    st.send(Ctl::request(json!({ "cmd": "new-browser-tab", "pane": pane, "url": files::file_url(&file) })));
                }
            }
        }
    }));

    agents_list.connect_row_activated({
        let state = state.clone();
        let ui = ui.clone();
        move |_, row| {
            let surface = row.widget_name().parse::<u64>().ok();
            if let Some(surface) = surface {
                show_surface_tab(&state, &ui, surface);
            }
        }
    });

    // While the files view shows and follows, keep it on the focused
    // terminal's working directory.
    glib::timeout_add_local(Duration::from_millis(1500), {
        let state = state.clone();
        let ui = ui.clone();
        move || {
            if ui.sidebar_stack.visible_child_name().as_deref() == Some("files") && ui.files.following() {
                let st = state.borrow();
                let surface = st
                    .target_pane()
                    .and_then(|pane| st.panes.get(&pane))
                    .filter(|v| !v.browser)
                    .and_then(|v| v.surface);
                if let Some(surface) = surface {
                    st.send(Ctl::ProcessInfo { surface });
                }
            }
            glib::ControlFlow::Continue
        }
    });

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
            let index = row.index();
            let id = state.borrow().workspaces.get(index as usize).map(|w| w.id);
            if id.is_some() && id != state.borrow().selected {
                // Client-local: other windows and the TUI keep their own.
                state.borrow_mut().selected = id;
                render(&state, &ui);
            }
        }
    });

    // Drop a tab on a workspace to move it there, or below the rows for a new
    // workspace. The move keeps the terminal or browser running.
    let sidebar_drop = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    sidebar_drop.connect_drop({
        let state = state.clone();
        move |target, value, _, y| {
            let Some(surface) = dragged_surface(value) else { return false };
            let list = target.widget().and_downcast::<gtk::ListBox>();
            let row = list.and_then(|l| l.row_at_y(y as i32));
            let st = state.borrow();
            let mut request = json!({ "cmd": "move-tab-to-workspace", "surface": surface });
            if let Some(ws) = row.and_then(|r| st.workspaces.get(r.index() as usize)) {
                request["workspace"] = json!(ws.id);
            }
            st.send(Ctl::request(request));
            true
        }
    });
    list.add_controller(sidebar_drop);

    list.connect_row_activated({
        let state = state.clone();
        move |list, row| {
            let ws = state.borrow().workspaces.get(row.index() as usize).cloned();
            let Some(ws) = ws else { return };
            let ctl = state.borrow().ctl.clone();
            rename_popover(list, row, &ws.name, move |name| {
                let _ = ctl.send(Ctl::request(json!({ "cmd": "rename-workspace", "workspace": ws.id, "name": name })));
            });
        }
    });

    install_shortcuts(&window, &state);

    // Report every shown terminal's cell grid when it changes. VTE resizes
    // its own grid to the allocation; we mirror it to the server.
    glib::timeout_add_local(Duration::from_millis(200), {
        let state = state.clone();
        move || {
            size_columns(&state);
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
                    UiMsg::UndoConfirm { pane, revision, closes } => confirm_undo(&state, pane, revision, closes),
                    UiMsg::Cwd(cwd) => ui.files.follow_to(std::path::Path::new(&cwd)),
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
    // Workspaces / Files sidebar view (also Ctrl+Shift+F).
    let toggle_files = gio::SimpleAction::new("toggle-files", None);
    toggle_files.connect_activate({
        let ui = ui.clone();
        move |_, _| {
            let next = if ui.sidebar_stack.visible_child_name().as_deref() == Some("files") { "workspaces" } else { "files" };
            ui.sidebar_stack.set_visible_child_name(next);
        }
    });
    app.add_action(&toggle_files);
    install_app_actions(app, &state, &ui);

    // Right-click a workspace in the sidebar: Rename / Close.
    let row_menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
    row_menu.set_parent(&ui.list);
    row_menu.set_has_arrow(false);
    let row_click = gtk::GestureClick::new();
    row_click.set_button(3);
    row_click.connect_pressed({
        let (state, list) = (state.clone(), ui.list.clone());
        move |_, _, x, y| {
            let Some(row) = list.row_at_y(y as i32) else { return };
            let Some(ws) = state.borrow().workspaces.get(row.index() as usize).map(|w| w.id) else { return };
            let model = gio::Menu::new();
            for (label, action) in [("Rename…", "app.rename-workspace"), ("Close workspace…", "app.close-workspace")] {
                let item = gio::MenuItem::new(Some(label), None);
                item.set_action_and_target_value(Some(action), Some(&ws.to_variant()));
                model.append_item(&item);
            }
            row_menu.set_menu_model(Some(&model));
            row_menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            row_menu.popup();
        }
    });
    ui.list.add_controller(row_click);

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

/// The whole style sheet: layout, badges and the theme's chrome colours.
fn full_css() -> String {
    format!("{CSS}{}{}", attention::css(), theme::chrome_css())
}

/// Restyle when the cmux config changes (a theme switched with
/// `cmux-theme`, in the TUI, or from this window). Waits for writes to
/// settle: `cmux-theme` rewrites the config and the Ghostty block.
fn watch_theme(provider: &gtk::CssProvider) -> Option<gio::FileMonitor> {
    let file = gio::File::for_path(theme::config_path()?);
    let monitor = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE).ok()?;
    let pending: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
    let provider = provider.clone();
    monitor.connect_changed(move |_, _, _, _| {
        if let Some(source) = pending.take() {
            source.remove();
        }
        let (provider, pending_inner) = (provider.clone(), pending.clone());
        pending.set(Some(glib::timeout_add_local_once(Duration::from_millis(500), move || {
            pending_inner.set(None);
            provider.load_from_string(&full_css());
        })));
    });
    Some(monitor)
}

/// Installed cmux themes (`~/.config/cmux/themes/*.json`), plus `default`.
fn installed_themes() -> Vec<String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|c| c.join("cmux/themes"));
    let mut names: Vec<String> = dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            (path.extension().and_then(|x| x.to_str()) == Some("json"))
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect();
    names.sort();
    names.push("default".into());
    names
}

/// The `cmux-theme` script (installed by scripts/40-install.sh), on PATH or
/// in ~/.local/bin (a desktop launch may not have that on PATH).
fn cmux_theme() -> PathBuf {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).map(|d| d.join("cmux-theme")).find(|p| p.is_file())
    });
    on_path.unwrap_or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/cmux-theme")).unwrap_or_default()
    })
}

fn current_theme() -> String {
    std::process::Command::new(cmux_theme())
        .arg("current")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into())
}

/// "sw-nord-dark" → "Sw nord dark"-style labels aren't nicer than the names
/// people type with `cmux-theme set`, so the menu shows the names as is.
fn build_main_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let themes = gio::Menu::new();
    for name in installed_themes() {
        let item = gio::MenuItem::new(Some(&name), None);
        item.set_action_and_target_value(Some("app.theme"), Some(&name.to_variant()));
        themes.append_item(&item);
    }
    let look = gio::Menu::new();
    look.append_submenu(Some("Theme"), &themes);
    look.append(Some("Workspaces / Files (Ctrl+Shift+F)"), Some("app.toggle-files"));
    menu.append_section(None, &look);
    let workspace = gio::Menu::new();
    workspace.append(Some("New workspace"), Some("app.new-workspace"));
    workspace.append(Some("Rename workspace…"), Some("app.rename-this-workspace"));
    workspace.append(Some("Close workspace…"), Some("app.close-this-workspace"));
    menu.append_section(None, &workspace);
    menu
}

/// App actions behind the main menu and the sidebar's right-click menu.
fn install_app_actions(app: &gtk::Application, state: &Shared, ui: &Ui) {
    // Theme: a radio group whose state is the active cmux theme. Switching
    // runs `cmux-theme set`, the same as the TUI's picker: it rewrites the
    // config and the Ghostty colour block, reloads every session (terminals
    // recolour through the server), and this window's badges follow.
    let theme = gio::SimpleAction::new_stateful("theme", Some(glib::VariantTy::STRING), &current_theme().to_variant());
    theme.connect_activate({
        let (state, ui) = (state.clone(), ui.clone());
        move |action, target| {
            let Some(name) = target.and_then(|t| t.get::<String>()) else { return };
            action.set_state(&name.to_variant());
            ui.status.set_text(&format!("Switching to theme {name}…"));
            let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE);
            let program = cmux_theme();
            let spawned = launcher.spawn(&[program.as_os_str(), std::ffi::OsStr::new("set"), std::ffi::OsStr::new(&name)]);
            let (state, ui) = (state.clone(), ui.clone());
            match spawned {
                Ok(process) => {
                    glib::spawn_future_local(async move {
                        match process.wait_check_future().await {
                            Ok(()) => {
                                if let Some(css) = state.borrow().css.as_ref() {
                                    css.load_from_string(&full_css());
                                }
                                ui.status.set_text(&format!("Theme: {name}"));
                            }
                            Err(error) => ui.status.set_text(&format!("cmux-theme failed: {error}")),
                        }
                    });
                }
                Err(error) => ui.status.set_text(&format!("Can't run cmux-theme ({}): {error}", program.display())),
            }
        }
    });
    app.add_action(&theme);

    let new_workspace = gio::SimpleAction::new("new-workspace", None);
    new_workspace.connect_activate({
        let state = state.clone();
        move |_, _| state.borrow().send(Ctl::NewWorkspace)
    });
    app.add_action(&new_workspace);

    // Rename / close a workspace by id (sidebar menu) or the shown one
    // (main menu).
    let rename = |state: &Shared, ui: &Ui, ws_id: u64| {
        let st = state.borrow();
        let Some(index) = st.workspaces.iter().position(|w| w.id == ws_id) else { return };
        let current = st.workspaces[index].name.clone();
        let ctl = st.ctl.clone();
        drop(st);
        let Some(row) = ui.list.row_at_index(index as i32) else { return };
        rename_popover(&ui.list, &row, &current, move |name| {
            let _ = ctl.send(Ctl::request(json!({ "cmd": "rename-workspace", "workspace": ws_id, "name": name })));
        });
    };
    for (name, by_id) in [("rename-workspace", true), ("rename-this-workspace", false), ("close-workspace", true), ("close-this-workspace", false)] {
        let action = gio::SimpleAction::new(name, by_id.then_some(glib::VariantTy::UINT64));
        let closing = name.starts_with("close");
        action.connect_activate({
            let (state, ui) = (state.clone(), ui.clone());
            move |_, target| {
                let ws_id = if by_id { target.and_then(|t| t.get::<u64>()) } else { state.borrow().selected };
                let Some(ws_id) = ws_id else { return };
                if closing {
                    close_with_shells(&state, Closing::Workspace(ws_id));
                } else {
                    rename(&state, &ui, ws_id);
                }
            }
        });
        app.add_action(&action);
    }
}

/// Something to close from the window.
enum Closing {
    Tab(u64),
    Pane(u64),
    Screen(u64),
    Workspace(u64),
}

/// Close a tab, pane, screen or workspace and end the shells it held. The
/// server's close commands only remove placements; without this the shells
/// would keep running unseen. A single tab closes at once; a pane or screen
/// holding two or more shells, and any workspace, asks first.
fn close_with_shells(state: &Shared, what: Closing) {
    let st = state.borrow();
    let all_screens = || st.raw.iter().flat_map(|ws| ws.screens.iter().map(move |s| (ws, s)));
    let (request, tabs, title, always_ask): (Value, Vec<backend::Tab>, String, bool) = match what {
        Closing::Tab(surface) => {
            let tab = st.raw.iter().flat_map(|ws| ws.all_tabs()).find(|t| t.surface == surface).cloned();
            (json!({ "cmd": "close-surface", "surface": surface }), tab.into_iter().collect(), String::new(), false)
        }
        Closing::Pane(pane) => {
            let tabs = all_screens()
                .flat_map(|(_, s)| &s.panes)
                .find(|p| p.id == pane)
                .map(|p| p.tabs.clone())
                .unwrap_or_default();
            (json!({ "cmd": "close-pane", "pane": pane }), tabs, "this pane".into(), false)
        }
        Closing::Screen(screen) => {
            let found = all_screens().find(|(_, s)| s.id == screen);
            let tabs = found.map(|(_, s)| s.panes.iter().flat_map(|p| p.tabs.clone()).collect()).unwrap_or_default();
            let title = found
                .and_then(|(ws, s)| {
                    let index = ws.screens.iter().position(|x| x.id == s.id)?;
                    Some(s.name.clone().map_or(format!("screen {}", index + 1), |n| format!("screen “{n}”")))
                })
                .unwrap_or_else(|| "this screen".into());
            (json!({ "cmd": "close-screen", "screen": screen }), tabs, title, false)
        }
        Closing::Workspace(id) => {
            let Some(ws) = st.raw.iter().find(|w| w.id == id) else { return };
            let name = if ws.name.is_empty() { format!("Workspace {}", ws.id) } else { ws.name.clone() };
            (json!({ "cmd": "close-workspace", "workspace": id }), ws.all_tabs().cloned().collect(), format!("workspace “{name}”"), true)
        }
    };
    let terminals: Vec<String> = tabs.iter().filter_map(|t| t.terminal_id.clone()).collect();
    let browsers = tabs.iter().filter(|t| !t.terminal).count();
    let ctl = st.ctl.clone();
    if !always_ask && terminals.len() < 2 {
        let _ = ctl.send(Ctl::Close { request, terminals });
        return;
    }
    let Some(window) = st.window.upgrade() else { return };
    drop(st);
    let mut detail = format!(
        "This ends the {} shell{} running there",
        terminals.len(),
        if terminals.len() == 1 { "" } else { "s" }
    );
    if browsers > 0 {
        detail.push_str(&format!(" and closes {browsers} browser tab{}", if browsers == 1 { "" } else { "s" }));
    }
    detail.push_str(". Anything still running in them stops.");
    let dialog = gtk::AlertDialog::builder()
        .message(format!("Close {title}?"))
        .detail(detail)
        .buttons(["Cancel", "Close"])
        .cancel_button(0)
        .default_button(0)
        .modal(true)
        .build();
    glib::spawn_future_local(async move {
        if let Ok(1) = dialog.choose_future(Some(&window)).await {
            let _ = ctl.send(Ctl::Close { request, terminals });
        }
    });
}

/// Ctrl+Shift+T new tab, Ctrl+Shift+W close tab, Ctrl+Shift+E split right,
/// Ctrl+Shift+O split down, Ctrl+PageDown/PageUp next/previous tab. They act
/// on the focused pane and run in the capture phase, ahead of VTE.
fn install_shortcuts(window: &gtk::ApplicationWindow, state: &Shared) {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Global);
    let bind = |accel: &str, action: fn(&Shared, u64)| {
        let state = state.clone();
        let callback = gtk::CallbackAction::new(move |_, _| {
            let pane = state.borrow().target_pane();
            if let Some(pane) = pane {
                action(&state, pane);
            }
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(accel), Some(callback)));
    };
    bind("<Control><Shift>t", |st, pane| st.borrow().send(new_tab(pane)));
    bind("<Control><Shift>e", |st, pane| st.borrow().send(split(pane, "right")));
    bind("<Control><Shift>o", |st, pane| st.borrow().send(split(pane, "down")));
    bind("<Control><Shift>b", |st, pane| st.borrow().send(new_browser_tab(pane)));
    bind("<Control><Shift>u", |st, pane| st.borrow().send(Ctl::Undo { pane, confirm: None }));
    bind("<Control><Shift>f", |st, _| {
        let ui = st.borrow().ui.clone();
        if let Some(ui) = ui {
            let next = if ui.sidebar_stack.visible_child_name().as_deref() == Some("files") { "workspaces" } else { "files" };
            ui.sidebar_stack.set_visible_child_name(next);
        }
    });
    bind("<Control><Shift>g", |st, pane| {
        st.borrow().send(Ctl::request(json!({ "cmd": "new-pane-right", "pane": pane })));
    });
    bind("<Control><Shift>z", |st, pane| {
        st.borrow().send(Ctl::request(json!({ "cmd": "zoom-pane", "pane": pane, "mode": "toggle" })));
    });
    bind("<Control><Shift>n", |st, _| {
        let st = st.borrow();
        if let Some(ws) = st.workspace() {
            st.send(Ctl::request(json!({ "cmd": "new-screen", "workspace": ws.id })));
        }
    });
    bind("<Control><Shift>Page_Down", |st, _| step_screen(st, 1));
    bind("<Control><Shift>Page_Up", |st, _| step_screen(st, -1));
    bind("<Control><Shift>w", |st, pane| {
        let surface = st.borrow().panes.get(&pane).and_then(|v| v.surface);
        if let Some(surface) = surface {
            close_with_shells(st, Closing::Tab(surface));
        }
    });
    bind("<Control>Page_Down", |st, pane| step_tab(st, pane, 1));
    bind("<Control>Page_Up", |st, pane| step_tab(st, pane, -1));
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
    // Follow the shared defaults only for things that are new since the last
    // tree (created here or elsewhere and made active by the server: a new
    // workspace, screen or tab); otherwise keep this window's own choices.
    let target = {
        let mut st = state.borrow_mut();
        let first = st.known.is_none();
        let (old_ws, old_screens, old_tabs) = st.known.take().unwrap_or_default();
        let mut wanted = st.selected;
        for ws in &workspaces {
            if (first && ws.active && wanted.is_none()) || (!first && ws.active && !old_ws.contains(&ws.id)) {
                wanted = Some(ws.id);
            }
            for screen in &ws.screens {
                if !first && screen.active && !old_screens.contains(&screen.id) {
                    st.screen_choice.insert(ws.id, screen.id);
                }
                for pane in &screen.panes {
                    if let Some(tab) = pane.active() {
                        if !first && !old_tabs.contains(&tab.surface) {
                            st.tab_choice.insert(pane.id, tab.surface);
                        }
                    }
                }
            }
        }
        st.known = Some((
            workspaces.iter().map(|w| w.id).collect(),
            workspaces.iter().flat_map(|w| &w.screens).map(|s| s.id).collect(),
            workspaces.iter().flat_map(|w| w.all_tabs()).map(|t| t.surface).collect(),
        ));
        // Forget choices for things that are gone.
        let screens: HashSet<u64> = workspaces.iter().flat_map(|w| &w.screens).map(|s| s.id).collect();
        st.screen_choice.retain(|_, screen| screens.contains(screen));
        let tabs: HashSet<u64> = workspaces.iter().flat_map(|w| w.all_tabs()).map(|t| t.surface).collect();
        st.tab_choice.retain(|_, surface| tabs.contains(surface));

        let target = workspaces
            .iter()
            .position(|w| Some(w.id) == wanted)
            .or_else(|| workspaces.iter().position(|w| w.active))
            .or(if workspaces.is_empty() { None } else { Some(0) });
        st.selected = target.map(|i| workspaces[i].id);
        st.raw = workspaces;
        st.workspaces = apply_choices(&st.raw, &st.screen_choice, &st.tab_choice);
        target
    };
    let workspaces = state.borrow().workspaces.clone();

    // Rebuild the sidebar rows. Selecting a row fires `row-selected`, whose
    // handler sees `selected` already set and does nothing.
    // Only rows: the placeholder and any rename popover stay.
    for child in content_children(&ui.list).into_iter().filter(|c| c.is::<gtk::ListBoxRow>()) {
        ui.list.remove(&child);
    }
    for ws in &workspaces {
        let row = sidebar_row(ws, &state.borrow().agents);
        ui.list.append(&row);
    }
    if let Some(row) = target.and_then(|i| ui.list.row_at_index(i as i32)) {
        ui.list.select_row(Some(&row));
    }
    draw_agents(state, ui);
    render(state, ui);
}

/// The agents queue: every agent across all workspaces, blocked first, then
/// finished, working, idle, unknown; the most recent change first within
/// each. Click one to jump to its tab. Hidden while there are no agents.
fn draw_agents(state: &Shared, ui: &Ui) {
    while let Some(row) = ui.agents_list.row_at_index(0) {
        ui.agents_list.remove(&row);
    }
    let st = state.borrow();
    let rank = |status: &str| match status {
        "blocked" => 0,
        "done" => 1,
        "working" => 2,
        "idle" => 3,
        _ => 4,
    };
    let mut agents: Vec<(u64, &backend::Agent)> = st.agents.iter().map(|(s, a)| (*s, a)).collect();
    agents.sort_by(|a, b| rank(&a.1.status).cmp(&rank(&b.1.status)).then(b.1.updated_at.cmp(&a.1.updated_at)));
    let mut shown = 0;
    for (surface, agent) in agents {
        // Where it lives: workspace name and tab title.
        let place = st.raw.iter().find_map(|ws| {
            ws.all_tabs().find(|t| t.surface == surface).map(|t| {
                let ws_name = if ws.name.is_empty() { format!("Workspace {}", ws.id) } else { ws.name.clone() };
                (ws_name, t.name.clone().unwrap_or_else(|| t.title.clone()))
            })
        });
        let Some((ws_name, title)) = place else { continue };
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        line.set_margin_start(6);
        line.set_margin_top(3);
        line.set_margin_bottom(3);
        let glyph = match agent.status.as_str() {
            "blocked" => attention::badge(Attn::Blocked, 1),
            "done" => attention::badge(Attn::Done, 1),
            "working" => attention::badge(Attn::Working, 1),
            other => {
                let label = gtk::Label::new(Some(if other == "idle" { "○" } else { "·" }));
                label.add_css_class("dim-label");
                label.set_tooltip_text(Some(if other == "idle" { "Agent idle" } else { "Agent state unknown" }));
                label
            }
        };
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let title_label = gtk::Label::new(Some(if title.is_empty() { "agent" } else { &title }));
        title_label.set_xalign(0.0);
        title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let place_label = gtk::Label::new(Some(&format!("{ws_name} · {}", agent.status)));
        place_label.set_xalign(0.0);
        place_label.add_css_class("dim-label");
        place_label.add_css_class("caption");
        text.append(&title_label);
        text.append(&place_label);
        line.append(&glyph);
        line.append(&text);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&line));
        row.set_widget_name(&surface.to_string());
        ui.agents_list.append(&row);
        shown += 1;
    }
    ui.agents_box.set_visible(shown > 0);
}

/// The server tree with this window's own screen and tab choices applied:
/// each workspace shows its chosen screen (else the shared active one) and
/// each pane its chosen tab.
fn apply_choices(raw: &[Workspace], screens: &HashMap<u64, u64>, tabs: &HashMap<u64, u64>) -> Vec<Workspace> {
    raw.iter()
        .map(|ws| {
            let mut ws = ws.clone();
            for screen in &mut ws.screens {
                for pane in &mut screen.panes {
                    if let Some(index) = tabs.get(&pane.id).and_then(|s| pane.tabs.iter().position(|t| t.surface == *s)) {
                        pane.active_tab = index;
                    }
                }
            }
            let shown = screens
                .get(&ws.id)
                .and_then(|id| ws.screens.iter().find(|s| s.id == *id))
                .or_else(|| ws.screens.iter().find(|s| s.active))
                .or_else(|| ws.screens.first())
                .cloned();
            if let Some(screen) = shown {
                ws.screen = Some(screen.id);
                ws.layout = screen.layout;
                ws.panes = screen.panes;
                ws.active_pane = screen.active_pane;
                ws.zoomed = screen.zoomed;
                ws.columns = screen.columns;
            }
            ws.title = ws
                .active_pane
                .and_then(|id| ws.panes.iter().find(|p| p.id == id))
                .and_then(|p| p.active())
                .map(|t| t.name.clone().unwrap_or_else(|| t.title.clone()))
                .unwrap_or_default();
            ws
        })
        .collect()
}

/// Re-derive the shown tree after a local selection change, and redraw.
fn reselect(state: &Shared) {
    let ui = {
        let mut st = state.borrow_mut();
        st.workspaces = apply_choices(&st.raw, &st.screen_choice, &st.tab_choice);
        st.ui.clone()
    };
    if let Some(ui) = ui {
        render(state, &ui);
    }
}

/// Show `screen` of the shown workspace (this window only).
fn choose_screen(state: &Shared, workspace: u64, screen: u64) {
    state.borrow_mut().screen_choice.insert(workspace, screen);
    reselect(state);
}

/// Show tab `surface` in `pane` (this window only).
fn choose_tab(state: &Shared, pane: u64, surface: u64) {
    state.borrow_mut().tab_choice.insert(pane, surface);
    reselect(state);
}

/// Step the shown screen or a pane's tab by `delta`, wrapping.
fn step_screen(state: &Shared, delta: isize) {
    let next = state.borrow().workspace().and_then(|ws| {
        let index = ws.screens.iter().position(|s| Some(s.id) == ws.screen)? as isize;
        let next = (index + delta).rem_euclid(ws.screens.len() as isize) as usize;
        Some((ws.id, ws.screens[next].id))
    });
    if let Some((workspace, screen)) = next {
        choose_screen(state, workspace, screen);
    }
}

fn step_tab(state: &Shared, pane: u64, delta: isize) {
    let next = state.borrow().workspace().and_then(|ws| {
        let pane = ws.panes.iter().find(|p| p.id == pane)?;
        if pane.tabs.is_empty() {
            return None;
        }
        let next = (pane.active_tab as isize + delta).rem_euclid(pane.tabs.len() as isize) as usize;
        Some(pane.tabs[next].surface)
    });
    if let Some(surface) = next {
        choose_tab(state, pane, surface);
    }
}

/// A container's children except popovers attached to it (an open rename
/// popover must survive the container being redrawn).
fn content_children(container: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
    let mut children = Vec::new();
    let mut next = container.first_child();
    while let Some(child) = next {
        next = child.next_sibling();
        if !child.is::<gtk::Popover>() {
            children.push(child);
        }
    }
    children
}

/// Edit a name in place: a popover with an entry, pointing at `target` but
/// parented to `host`, a widget that outlives redraws of `target` (sidebar
/// rows and strip buttons are rebuilt on every tree change). Enter saves; an
/// empty name clears it back to the default; Escape cancels.
fn rename_popover(
    host: &impl IsA<gtk::Widget>,
    target: &impl IsA<gtk::Widget>,
    current: &str,
    save: impl Fn(String) + 'static,
) {
    let popover = gtk::Popover::new();
    popover.set_parent(host);
    if let Some(bounds) = target.compute_bounds(host) {
        popover.set_pointing_to(Some(&gdk::Rectangle::new(
            bounds.x() as i32,
            bounds.y() as i32,
            bounds.width() as i32,
            bounds.height() as i32,
        )));
    }
    let entry = gtk::Entry::new();
    entry.set_text(current);
    entry.set_width_chars(24);
    entry.set_placeholder_text(Some("Name (empty for default)"));
    popover.set_child(Some(&entry));
    entry.connect_activate({
        let popover = popover.clone();
        move |entry| {
            save(entry.text().trim().to_string());
            popover.popdown();
        }
    });
    // Detach once closed (from the idle loop: not inside the signal).
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
    entry.grab_focus();
    entry.select_region(0, -1);
}

/// Runs `on_double` when `widget` is double-clicked (single clicks still do
/// whatever the widget does).
fn on_double_click(widget: &impl IsA<gtk::Widget>, on_double: impl Fn() + 'static) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(1);
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    gesture.connect_pressed(move |_, presses, _, _| {
        if presses == 2 {
            on_double();
        }
    });
    widget.add_controller(gesture);
}

/// The screen strip: one button per screen (number, name, attention badge),
/// `×` to close one, `+` for a new one. Which screen is shown is this
/// window's own choice.
fn draw_screens(state: &Shared, ui: &Ui, ws: Option<&Workspace>) {
    for child in content_children(&ui.screens) {
        ui.screens.remove(&child);
    }
    let Some(ws) = ws else {
        ui.screens.set_visible(false);
        return;
    };
    ui.screens.set_visible(true);
    let (ctl, agents) = {
        let st = state.borrow();
        (st.ctl.clone(), st.agents.clone())
    };
    let send = move |request: Value| {
        let _ = ctl.send(Ctl::request(request));
    };
    for (index, screen) in ws.screens.iter().enumerate() {
        let label = match &screen.name {
            Some(name) => format!("{} {name}", index + 1),
            None => format!("Screen {}", index + 1),
        };
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let marks: Vec<Attn> =
            screen.panes.iter().flat_map(|p| &p.tabs).filter_map(|t| attention::of_tab(t, &agents)).collect();
        if let Some(top) = marks.iter().copied().max() {
            inner.append(&attention::badge(top, marks.iter().filter(|a| a.needs_you()).count()));
        }
        inner.append(&gtk::Label::new(Some(&label)));
        let button = gtk::Button::new();
        button.set_child(Some(&inner));
        button.add_css_class("flat");
        button.set_focus_on_click(false);
        if Some(screen.id) == ws.screen {
            button.add_css_class("tab-active");
        }
        button.set_tooltip_text(Some("Switch screen (Ctrl+Shift+PageDown/PageUp)"));
        button.connect_clicked({
            let (state, workspace, screen_id) = (state.clone(), ws.id, screen.id);
            move |_| choose_screen(&state, workspace, screen_id)
        });
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_focus_on_click(false);
        close.set_tooltip_text(Some("Close screen (ends the shells on it)"));
        let screen_id = screen.id;
        close.connect_clicked({
            let state = state.clone();
            move |_| close_with_shells(&state, Closing::Screen(screen_id))
        });
        on_double_click(&button, {
            let (strip, button, send, current) =
                (ui.screens.clone(), button.clone(), send.clone(), screen.name.clone().unwrap_or_default());
            move || {
                let send = send.clone();
                rename_popover(&strip, &button, &current, move |name| {
                    send(json!({ "cmd": "rename-screen", "screen": screen_id, "name": name }));
                });
            }
        });
        button.set_tooltip_text(Some("Switch screen (Ctrl+Shift+PageDown/PageUp) · double-click to rename"));
        let group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        group.append(&button);
        group.append(&close);
        ui.screens.append(&group);
    }
    let add = gtk::Button::with_label("+");
    add.add_css_class("flat");
    add.set_focus_on_click(false);
    add.set_tooltip_text(Some("New screen (Ctrl+Shift+N)"));
    let workspace = ws.id;
    add.connect_clicked(move |_| send(json!({ "cmd": "new-screen", "workspace": workspace })));
    ui.screens.append(&add);
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
        ws.all_tabs().filter_map(|t| attention::of_tab(t, agents)).collect();
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
        draw_screens(state, ui, None);
        ui.status.set_text("Session has no workspaces");
        return;
    };
    draw_screens(state, ui, Some(&ws));
    let Some(layout) = ws.layout.clone() else {
        clear_content(state, ui);
        ui.status.set_text("This workspace has no panes");
        return;
    };
    // A zoomed pane is shown alone; the split tree is kept for unzooming.
    let zoomed = ws.zoomed.filter(|pane| layout.panes().contains(pane));
    let layout = match zoomed {
        Some(pane) => Layout::Leaf(pane),
        None => layout,
    };
    // Viewport columns, unless a pane is zoomed (then it's shown alone).
    let columns = if zoomed.is_some() { Vec::new() } else { ws.columns.clone() };

    let shape = if columns.is_empty() {
        layout.signature()
    } else {
        let cols: Vec<String> = columns.iter().map(|c| format!("{}={}", c.id, c.layout.signature())).collect();
        format!("cols[{}]", cols.join(","))
    };
    let signature = format!("{}:{}:{}", ws.id, ws.screen.unwrap_or(0), shape);
    let rebuilt = state.borrow().built.as_deref() != Some(&signature);
    if rebuilt {
        if columns.is_empty() {
            rebuild(state, ui, &layout);
        } else {
            rebuild_columns(state, ui, &columns);
        }
        state.borrow_mut().built = Some(signature);
    }

    if columns.is_empty() {
        sync_ratios(state, &layout);
    } else {
        sync_columns(state, &columns);
        for column in &columns {
            sync_ratios(state, &column.layout);
        }
    }
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
    let mut name = if ws.name.is_empty() { format!("workspace {}", ws.id) } else { ws.name.clone() };
    if ws.screens.len() > 1 {
        let index = ws.screens.iter().position(|s| Some(s.id) == ws.screen).unwrap_or(0);
        name = format!("{name} · screen {} of {}", index + 1, ws.screens.len());
    }
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
    st.viewport = None;
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
        st.viewport = None;
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
    } else if let Some(scroller) = widget.downcast_ref::<gtk::ScrolledWindow>() {
        if let Some(child) = scroller.child() {
            dismantle(&child);
        }
        scroller.set_child(None::<&gtk::Widget>);
    } else if let Some(viewport) = widget.downcast_ref::<gtk::Viewport>() {
        if let Some(child) = viewport.child() {
            dismantle(&child);
        }
        viewport.set_child(None::<&gtk::Widget>);
    } else if let Some(container) = widget.downcast_ref::<gtk::Box>() {
        // Only the column row and columns; a pane's own box stays intact.
        if container.has_css_class("viewport-row") || container.has_css_class("viewport-column") {
            for child in content_children(container) {
                dismantle(&child);
                container.remove(&child);
            }
        }
    }
}

/// Build a screen that uses viewport columns: a horizontally scrolling row
/// of columns, each its fraction of the visible width, each with its own
/// split tree, and a drag handle on its right edge to resize it.
fn rebuild_columns(state: &Shared, ui: &Ui, columns: &[Column]) {
    if let Some(child) = ui.content.first_child() {
        dismantle(&child);
        ui.content.remove(&child);
    }
    let wanted: Vec<u64> = columns.iter().flat_map(|c| c.layout.panes()).collect();
    {
        let mut st = state.borrow_mut();
        st.viewport = None;
        st.splits.clear();
        st.panes.retain(|id, _| wanted.contains(id));
    }
    for id in &wanted {
        if !state.borrow().panes.contains_key(id) {
            let view = new_pane_view(state, *id);
            state.borrow_mut().panes.insert(*id, view);
        }
    }

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class("viewport-row");
    let viewport = gtk::Viewport::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    viewport.set_child(Some(&row));
    let scroller = gtk::ScrolledWindow::builder()
        .child(&viewport)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .vexpand(true)
        .build();

    let ctl = state.borrow().ctl.clone();
    let mut views = Vec::new();
    for column in columns {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        widget.add_css_class("viewport-column");
        let inner = build_layout(state, &column.layout);
        inner.set_hexpand(true);
        widget.append(&inner);
        row.append(&widget);

        // Right-edge handle: drag to resize, sent on release.
        let handle = gtk::Separator::new(gtk::Orientation::Vertical);
        handle.add_css_class("column-handle");
        handle.set_cursor_from_name(Some("col-resize"));
        let fraction = Rc::new(Cell::new(column.width));
        let dragging = Rc::new(Cell::new(false));
        let drag = gtk::GestureDrag::new();
        let start = Rc::new(Cell::new(0.0));
        drag.connect_drag_begin({
            let (widget, start, dragging) = (widget.clone(), start.clone(), dragging.clone());
            move |_, _, _| {
                start.set(f64::from(widget.width()));
                dragging.set(true);
            }
        });
        drag.connect_drag_update({
            let (widget, scroller, start, fraction) = (widget.clone(), scroller.clone(), start.clone(), fraction.clone());
            move |_, dx, _| {
                let total = f64::from(scroller.width().max(1));
                let value = ((start.get() + dx) / total).clamp(0.1, 1.0);
                fraction.set(value);
                widget.set_size_request((value * total).round() as i32, -1);
            }
        });
        let pane = column.layout.panes().first().copied();
        drag.connect_drag_end({
            let (fraction, dragging, ctl) = (fraction.clone(), dragging.clone(), ctl.clone());
            move |_, _, _| {
                dragging.set(false);
                if let Some(pane) = pane {
                    let request = json!({ "cmd": "set-viewport-pane-width", "pane": pane, "width": fraction.get() });
                    let _ = ctl.send(Ctl::request(request));
                }
            }
        });
        handle.add_controller(drag);
        row.append(&handle);
        views.push(ColumnView { id: column.id, widget, fraction, dragging });
    }
    ui.content.append(&scroller);
    state.borrow_mut().viewport = Some(ViewportView { scroller, columns: views });
    size_columns(state);
}

/// Take column widths from the tree (unless one is being dragged), then
/// size them.
fn sync_columns(state: &Shared, columns: &[Column]) {
    if let Some(viewport) = state.borrow().viewport.as_ref() {
        for view in &viewport.columns {
            if let Some(column) = columns.iter().find(|c| c.id == view.id) {
                if !view.dragging.get() {
                    view.fraction.set(column.width);
                }
            }
        }
    }
    size_columns(state);
}

/// Size each column to its fraction of the visible width (re-run as the
/// window resizes).
fn size_columns(state: &Shared) {
    let st = state.borrow();
    let Some(viewport) = st.viewport.as_ref() else { return };
    let total = viewport.scroller.width();
    if total <= 0 {
        return;
    }
    for view in &viewport.columns {
        let width = (view.fraction.get() * f64::from(total)).round() as i32;
        if view.widget.width_request() != width {
            view.widget.set_size_request(width, -1);
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

    // Clipboard: Ctrl+Shift+C copies the selection, Ctrl+Shift+V and
    // Shift+Insert paste (plain Ctrl+C/V belong to the program running).
    // VTE pastes through the same `commit` path as typing, so the text
    // reaches the session with bracketed paste respected; middle-click
    // pastes the primary selection the same way.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let term = term.clone();
        move |_, key, _, state| {
            let ctrl_shift = state.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK);
            let lower = key.to_lower();
            if ctrl_shift && lower == gdk::Key::c {
                if term.has_selection() {
                    term.copy_clipboard_format(vte4::Format::Text);
                }
                glib::Propagation::Stop
            } else if (ctrl_shift && lower == gdk::Key::v)
                || (state.contains(gdk::ModifierType::SHIFT_MASK) && key == gdk::Key::Insert)
            {
                term.paste_clipboard();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    term.add_controller(keys);

    let term_actions = gio::SimpleActionGroup::new();
    for name in ["copy", "paste", "select-all"] {
        let action = gio::SimpleAction::new(name, None);
        let term = term.clone();
        action.connect_activate(move |_, _| match name {
            "copy" => {
                if term.has_selection() {
                    term.copy_clipboard_format(vte4::Format::Text);
                }
            }
            "paste" => term.paste_clipboard(),
            _ => term.select_all(),
        });
        term_actions.add_action(&action);
    }
    term.insert_action_group("term", Some(&term_actions));

    // Right-click: clipboard and pane actions. Shift+right-click goes to the
    // program instead (for terminal apps that use the mouse).
    let term_menu = gtk::PopoverMenu::from_model(Some(&{
        let menu = gio::Menu::new();
        let edit = gio::Menu::new();
        edit.append(Some("Copy (Ctrl+Shift+C)"), Some("term.copy"));
        edit.append(Some("Paste (Ctrl+Shift+V)"), Some("term.paste"));
        edit.append(Some("Select all"), Some("term.select-all"));
        menu.append_section(None, &edit);
        let pane = gio::Menu::new();
        pane.append(Some("New tab"), Some("pane.new-tab"));
        pane.append(Some("Split right"), Some("pane.split-right"));
        pane.append(Some("Split down"), Some("pane.split-down"));
        pane.append(Some("Zoom / unzoom"), Some("pane.zoom"));
        menu.append_section(None, &pane);
        menu
    }));
    term_menu.set_parent(&term);
    term_menu.set_has_arrow(false);
    let right_click = gtk::GestureClick::new();
    right_click.set_button(3);
    right_click.set_propagation_phase(gtk::PropagationPhase::Capture);
    right_click.connect_pressed({
        let term_menu = term_menu.clone();
        move |gesture, _, x, y| {
            if gesture.current_event_state().contains(gdk::ModifierType::SHIFT_MASK) {
                return; // to the program
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            term_menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            term_menu.popup();
        }
    });
    term.add_controller(right_click);

    // Focusing a terminal makes its pane the shortcut target (client-local).
    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter({
        let state = state.clone();
        move |_| {
            // Client-local, like the rest of the selection.
            let changed = state.borrow().focused != Some(id);
            if changed {
                state.borrow_mut().focused = Some(id);
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
    install_pane_actions(state, &root, &tabbar, id);
    install_tab_drop(state, &tabbar, &tabbar, id);
    install_tab_drop(state, &stack, &tabbar, id);
    root.append(&tabbar);
    root.append(&stack);

    PaneView {
        root,
        term_menu,
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
    for child in content_children(tabbar) {
        tabbar.remove(&child);
    }
    let ctl = state.borrow().ctl.clone();
    let send = move |ctl_msg: Ctl| {
        let _ = ctl.send(ctl_msg);
    };
    for (index, tab) in pane.tabs.iter().enumerate() {
        let title = match (&tab.name, tab.title.is_empty()) {
            (Some(name), _) => name.clone(),
            (None, true) => format!("Tab {}", index + 1),
            (None, false) => tab.title.clone(),
        };
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
            let state = state.clone();
            move |_| choose_tab(&state, pane_id, surface)
        });
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_tooltip_text(Some("Close tab and end its shell (Ctrl+Shift+W)"));
        close.connect_clicked({
            let state = state.clone();
            move |_| close_with_shells(&state, Closing::Tab(surface))
        });
        on_double_click(&button, {
            let (tabbar, button, send, current) =
                (tabbar.clone(), button.clone(), send.clone(), tab.name.clone().unwrap_or_default());
            move || {
                let send = send.clone();
                rename_popover(&tabbar, &button, &current, move |name| {
                    send(Ctl::request(json!({ "cmd": "rename-surface", "surface": surface, "name": name })));
                });
            }
        });
        // Drag the tab to reorder it, move it to another pane, or drop it on
        // a workspace in the sidebar.
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::MOVE);
        drag.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&format!("{TAB_DRAG_PREFIX}{surface}").to_value()))
        });
        drag.connect_drag_begin({
            let button = button.clone();
            move |source, _| source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&button))), 0, 0)
        });
        button.add_controller(drag);
        let group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        group.add_css_class("tab-group");
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

    // Pane menu: zoom, swap, rename, close (actions in the pane's `pane.`
    // group, see `install_pane_actions`).
    let menu = gio::Menu::new();
    let view = gio::Menu::new();
    view.append(Some("Zoom / unzoom (Ctrl+Shift+Z)"), Some("pane.zoom"));
    view.append(Some("New column to the right (Ctrl+Shift+G)"), Some("pane.new-column"));
    menu.append_section(None, &view);
    let swap = gio::Menu::new();
    for (label, dir) in [("Swap with left", "left"), ("Swap with right", "right"), ("Swap with above", "up"), ("Swap with below", "down")] {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some("pane.swap"), Some(&dir.to_variant()));
        swap.append_item(&item);
    }
    menu.append_section(None, &swap);
    let manage = gio::Menu::new();
    manage.append(Some("Undo layout change (Ctrl+Shift+U)"), Some("pane.undo"));
    manage.append(Some("Move tab to new workspace"), Some("pane.move-new-workspace"));
    manage.append(Some("Rename tab…"), Some("pane.rename"));
    manage.append(Some("Close pane"), Some("pane.close"));
    menu.append_section(None, &manage);
    let more = gtk::MenuButton::new();
    more.set_icon_name("view-more-symbolic");
    more.add_css_class("flat");
    more.set_tooltip_text(Some("Pane menu"));
    more.set_menu_model(Some(&menu));
    tabbar.append(&more);
}

/// The `pane.` actions behind a pane's menu.
fn install_pane_actions(state: &Shared, root: &gtk::Box, tabbar: &gtk::Box, id: u64) {
    let group = gio::SimpleActionGroup::new();
    let ctl = state.borrow().ctl.clone();
    let send = move |request: Value| {
        let _ = ctl.send(Ctl::request(request));
    };
    let zoom = gio::SimpleAction::new("zoom", None);
    zoom.connect_activate({
        let send = send.clone();
        move |_, _| send(json!({ "cmd": "zoom-pane", "pane": id, "mode": "toggle" }))
    });
    group.add_action(&zoom);
    for (name, request) in [
        ("new-tab", json!({ "cmd": "new-tab", "pane": id })),
        ("split-right", json!({ "cmd": "split", "pane": id, "dir": "right" })),
        ("split-down", json!({ "cmd": "split", "pane": id, "dir": "down" })),
    ] {
        let action = gio::SimpleAction::new(name, None);
        let send = send.clone();
        action.connect_activate(move |_, _| send(request.clone()));
        group.add_action(&action);
    }
    let new_column = gio::SimpleAction::new("new-column", None);
    new_column.connect_activate({
        let send = send.clone();
        move |_, _| send(json!({ "cmd": "new-pane-right", "pane": id }))
    });
    group.add_action(&new_column);
    let swap = gio::SimpleAction::new("swap", Some(glib::VariantTy::STRING));
    swap.connect_activate({
        let send = send.clone();
        move |_, dir| {
            if let Some(dir) = dir.and_then(|d| d.get::<String>()) {
                send(json!({ "cmd": "swap-pane", "pane": id, "dir": dir }));
            }
        }
    });
    group.add_action(&swap);
    let rename = gio::SimpleAction::new("rename", None);
    rename.connect_activate({
        let (state, tabbar, send) = (state.clone(), tabbar.clone(), send.clone());
        move |_, _| {
            // The pane's active tab.
            let active = {
                let st = state.borrow();
                st.workspace()
                    .and_then(|ws| ws.panes.iter().find(|p| p.id == id))
                    .and_then(|pane| pane.active())
                    .map(|tab| (tab.surface, tab.name.clone().unwrap_or_default()))
            };
            let Some((surface, current)) = active else { return };
            let send = send.clone();
            rename_popover(&tabbar, &tabbar, &current, move |name| {
                send(json!({ "cmd": "rename-surface", "surface": surface, "name": name }));
            });
        }
    });
    group.add_action(&rename);
    let undo = gio::SimpleAction::new("undo", None);
    undo.connect_activate({
        let state = state.clone();
        move |_, _| state.borrow().send(Ctl::Undo { pane: id, confirm: None })
    });
    group.add_action(&undo);
    let move_new = gio::SimpleAction::new("move-new-workspace", None);
    move_new.connect_activate({
        let (state, send) = (state.clone(), send.clone());
        move |_, _| {
            let surface = state
                .borrow()
                .workspace()
                .and_then(|ws| ws.panes.iter().find(|p| p.id == id))
                .and_then(|p| p.active())
                .map(|t| t.surface);
            if let Some(surface) = surface {
                // No workspace: the server creates one and selects it.
                send(json!({ "cmd": "move-tab-to-workspace", "surface": surface }));
            }
        }
    });
    group.add_action(&move_new);
    let close = gio::SimpleAction::new("close", None);
    close.connect_activate({
        let state = state.clone();
        move |_, _| close_with_shells(&state, Closing::Pane(id))
    });
    group.add_action(&close);
    root.insert_action_group("pane", Some(&group));
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
        let done = st.agents.get(&tab.surface).is_some_and(|a| a.status == "done");
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
    // Find it on any screen of any workspace: (workspace index and id,
    // screen index, pane, tab index).
    let found = state.borrow().workspaces.iter().enumerate().find_map(|(ws_index, ws)| {
        ws.screens.iter().enumerate().find_map(|(screen_index, screen)| {
            screen.panes.iter().find_map(|pane| {
                let index = pane.tabs.iter().position(|t| t.surface == surface)?;
                Some((ws_index, ws.id, screen_index, pane.id, index))
            })
        })
    });
    let Some((ws_index, ws_id, screen_index, pane_id, index)) = found else { return };
    {
        let mut st = state.borrow_mut();
        st.selected = Some(ws_id);
        st.focused = Some(pane_id);
        let screen_id = st.raw.get(ws_index).and_then(|ws| ws.screens.get(screen_index)).map(|s| s.id);
        if let Some(screen_id) = screen_id {
            st.screen_choice.insert(ws_id, screen_id);
        }
        let surface = st.raw.get(ws_index).and_then(|ws| ws.screens.get(screen_index)).and_then(|s| {
            s.panes.iter().find(|p| p.id == pane_id).and_then(|p| p.tabs.get(index)).map(|t| t.surface)
        });
        if let Some(surface) = surface {
            st.tab_choice.insert(pane_id, surface);
        }
        st.workspaces = apply_choices(&st.raw, &st.screen_choice, &st.tab_choice);
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

/// Undoing would close panes: say how many and confirm (with the preview's
/// revision, so a layout that changed meanwhile is refused, not undone).
fn confirm_undo(state: &Shared, pane: u64, revision: u64, closes: usize) {
    let Some(window) = state.borrow().window.upgrade() else { return };
    let dialog = gtk::AlertDialog::builder()
        .message("Undo the last layout change?")
        .detail(format!(
            "This closes {closes} pane{}. Terminals keep running in the session, but those panes and their tab placement go away.",
            if closes == 1 { "" } else { "s" }
        ))
        .buttons(["Cancel", "Undo"])
        .cancel_button(0)
        .default_button(1)
        .modal(true)
        .build();
    let ctl = state.borrow().ctl.clone();
    glib::spawn_future_local(async move {
        if let Ok(1) = dialog.choose_future(Some(&window)).await {
            let _ = ctl.send(Ctl::Undo { pane, confirm: Some(revision) });
        }
    });
}

/// Drag payload for a tab: its surface id.
const TAB_DRAG_PREFIX: &str = "cmux-tab:";

fn dragged_surface(value: &glib::Value) -> Option<u64> {
    value.get::<String>().ok()?.strip_prefix(TAB_DRAG_PREFIX)?.parse().ok()
}

/// Accept tab drops on a pane: on its tab strip at the position dropped, or
/// on its body at the end. Moving within the same pane reorders.
fn install_tab_drop(state: &Shared, target_widget: &impl IsA<gtk::Widget>, tabbar: &gtk::Box, pane: u64) {
    let drop = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    let (ctl, tabbar, on_strip) = (state.borrow().ctl.clone(), tabbar.clone(), target_widget.as_ref() == tabbar.upcast_ref::<gtk::Widget>());
    drop.connect_drop(move |_, value, x, _| {
        let Some(surface) = dragged_surface(value) else { return false };
        // Index among this pane's tabs: those whose middle is left of the
        // drop point (the server reads it against the pre-move list).
        let groups: Vec<gtk::Widget> =
            content_children(&tabbar).into_iter().filter(|w| w.has_css_class("tab-group")).collect();
        let index = if on_strip {
            groups
                .iter()
                .filter(|g| g.compute_bounds(&tabbar).is_some_and(|b| f64::from(b.x() + b.width() / 2.0) < x))
                .count()
        } else {
            groups.len()
        };
        let _ = ctl.send(Ctl::request(json!({ "cmd": "move-tab", "surface": surface, "pane": pane, "index": index })));
        true
    });
    target_widget.add_controller(drop);
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
