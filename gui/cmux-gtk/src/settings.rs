//! The window's own settings, and the settings window (Ctrl+, or ☰ →
//! Settings).
//!
//! Settings live in `~/.config/cmux-lin/gtk.json` and apply at once. The
//! theme and text size are shown here too but stored where they already
//! were (the cmux config via `cmux-theme`, and the text-size state file).
//! The session's own settings (keys, sidebar, status bar, commands) stay in
//! `cmux-tui.json`; the window has buttons to open that and the other
//! config files.

use std::path::PathBuf;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use serde_json::{Value, json};

/// The settings stored in `gtk.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Pango font description, e.g. `Monospace 11`.
    pub font: String,
    pub scrollback: u32,
    /// `block`, `ibeam` or `underline`.
    pub cursor_shape: String,
    pub cursor_blink: bool,
    pub desktop_notifications: bool,
    /// The branch and ports line under each workspace.
    pub sidebar_details: bool,
    /// The branch's pull request in that line (from `gh`).
    pub pull_requests: bool,
    /// Run the built-in browser provider.
    pub browser_provider: bool,
    /// The browser it uses; empty for automatic.
    pub browser: String,
    /// Start the session if it isn't running when the window opens.
    pub auto_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font: "Monospace 11".into(),
            scrollback: 10_000,
            cursor_shape: "block".into(),
            cursor_blink: true,
            desktop_notifications: true,
            sidebar_details: true,
            pull_requests: true,
            browser_provider: true,
            browser: String::new(),
            auto_start: true,
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|dir| dir.join("cmux-lin/gtk.json"))
}

impl Settings {
    /// The saved settings; anything missing or invalid keeps its default.
    pub fn load() -> Self {
        let mut settings = Self::default();
        let Some(json) = path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return settings;
        };
        let text = |key: &str| json.get(key).and_then(Value::as_str).map(str::to_string);
        let flag = |key: &str| json.get(key).and_then(Value::as_bool);
        if let Some(font) = text("font").filter(|f| !f.trim().is_empty()) {
            settings.font = font;
        }
        if let Some(lines) = json.get("scrollback").and_then(Value::as_u64) {
            settings.scrollback = lines.min(1_000_000) as u32;
        }
        if let Some(shape) = text("cursor_shape").filter(|s| ["block", "ibeam", "underline"].contains(&s.as_str())) {
            settings.cursor_shape = shape;
        }
        if let Some(browser) = text("browser") {
            settings.browser = browser;
        }
        for (key, slot) in [
            ("cursor_blink", &mut settings.cursor_blink),
            ("desktop_notifications", &mut settings.desktop_notifications),
            ("sidebar_details", &mut settings.sidebar_details),
            ("pull_requests", &mut settings.pull_requests),
            ("browser_provider", &mut settings.browser_provider),
            ("auto_start", &mut settings.auto_start),
        ] {
            if let Some(value) = flag(key) {
                *slot = value;
            }
        }
        settings
    }

    pub fn save(&self) -> Result<(), String> {
        let path = path().ok_or("no config folder")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = json!({
            "font": self.font,
            "scrollback": self.scrollback,
            "cursor_shape": self.cursor_shape,
            "cursor_blink": self.cursor_blink,
            "desktop_notifications": self.desktop_notifications,
            "sidebar_details": self.sidebar_details,
            "pull_requests": self.pull_requests,
            "browser_provider": self.browser_provider,
            "browser": self.browser,
            "auto_start": self.auto_start,
        });
        let text = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{text}\n")).map_err(|e| e.to_string())
    }

    /// Apply the terminal settings to one terminal.
    pub fn apply_to(&self, term: &vte4::Terminal) {
        use vte4::prelude::*;
        term.set_font(Some(&gtk::pango::FontDescription::from_string(&self.font)));
        term.set_scrollback_lines(i64::from(self.scrollback));
        term.set_cursor_shape(match self.cursor_shape.as_str() {
            "ibeam" => vte4::CursorShape::Ibeam,
            "underline" => vte4::CursorShape::Underline,
            _ => vte4::CursorShape::Block,
        });
        term.set_cursor_blink_mode(if self.cursor_blink { vte4::CursorBlinkMode::On } else { vte4::CursorBlinkMode::Off });
    }
}

/// What the settings window asks of the main window.
pub enum Request {
    /// The stored settings changed (already saved).
    Changed(Settings),
    Theme(String),
    /// Text size, as a scale (1.0 = 100%).
    TextScale(f64),
    /// Ask the session to re-read its config files.
    ReloadConfig,
    /// Import the cookies of this browser profile (index into the sources).
    ImportCookies(usize),
}

pub type Host = Rc<dyn Fn(Request)>;

/// What the window shows besides the stored settings.
pub struct Context {
    pub themes: Vec<String>,
    pub theme: String,
    pub text_scale: f64,
    /// Config files to offer: label and path.
    pub files: Vec<(String, PathBuf)>,
    /// Browser profiles cookies can be imported from.
    pub cookie_sources: Vec<String>,
}

/// Build the settings window.
pub fn window(parent: &gtk::ApplicationWindow, initial: Settings, context: Context, host: Host) -> gtk::Window {
    let window = gtk::Window::builder()
        .title("cmux settings")
        .transient_for(parent)
        .default_width(580)
        .default_height(680)
        .build();
    window.add_css_class("cmux");
    window.add_css_class("cmux-settings");
    let close = gtk::EventControllerKey::new();
    close.connect_key_pressed({
        let window = window.clone();
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(close);

    let current = Rc::new(std::cell::RefCell::new(initial.clone()));
    // Change one setting, save, tell the host.
    let change = {
        let (current, host, window) = (current.clone(), host.clone(), window.downgrade());
        move |edit: &dyn Fn(&mut Settings)| {
            let snapshot = {
                let mut settings = current.borrow_mut();
                edit(&mut settings);
                settings.clone()
            };
            if let Err(error) = snapshot.save() {
                if let Some(window) = window.upgrade() {
                    window.set_title(Some(&format!("cmux settings — couldn't save: {error}")));
                }
            }
            host(Request::Changed(snapshot));
        }
    };
    let change: Rc<dyn Fn(&dyn Fn(&mut Settings))> = Rc::new(change);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.set_margin_top(18);
    page.set_margin_bottom(18);
    page.set_margin_start(18);
    page.set_margin_end(18);

    // --- Appearance ---------------------------------------------------
    let section = new_section(&page, "Appearance");
    let themes = gtk::StringList::new(&context.themes.iter().map(String::as_str).collect::<Vec<_>>());
    let theme = gtk::DropDown::new(Some(themes), gtk::Expression::NONE);
    if let Some(index) = context.themes.iter().position(|t| *t == context.theme) {
        theme.set_selected(index as u32);
    }
    theme.connect_selected_notify({
        let (host, names) = (host.clone(), context.themes.clone());
        move |dropdown| {
            if let Some(name) = names.get(dropdown.selected() as usize) {
                host(Request::Theme(name.clone()));
            }
        }
    });
    row(&section, "Theme", "Colours for the window, the TUI and every terminal", &theme);

    let font = gtk::FontDialogButton::new(Some(gtk::FontDialog::builder().title("Terminal font").modal(true).build()));
    font.set_level(gtk::FontLevel::Font);
    font.set_font_desc(&gtk::pango::FontDescription::from_string(&initial.font));
    font.connect_font_desc_notify({
        let change = change.clone();
        move |button| {
            if let Some(desc) = button.font_desc() {
                let font = desc.to_str().to_string();
                change(&|s| s.font = font.clone());
            }
        }
    });
    row(&section, "Terminal font", "A monospaced font works best", &font);

    let scale = gtk::SpinButton::with_range(50.0, 300.0, 10.0);
    scale.set_value((context.text_scale * 100.0).round());
    scale.connect_value_changed({
        let host = host.clone();
        move |spin| host(Request::TextScale(spin.value() / 100.0))
    });
    row(&section, "Text size (%)", "Also Ctrl++, Ctrl+− and Ctrl+0", &scale);

    // --- Terminal -----------------------------------------------------
    let section = new_section(&page, "Terminal");
    let scrollback = gtk::SpinButton::with_range(0.0, 1_000_000.0, 1000.0);
    scrollback.set_value(f64::from(initial.scrollback));
    scrollback.connect_value_changed({
        let change = change.clone();
        move |spin| {
            let lines = spin.value() as u32;
            change(&|s| s.scrollback = lines);
        }
    });
    row(&section, "Scrollback lines", "History each terminal keeps here for scrolling and find", &scrollback);

    let shapes = ["block", "ibeam", "underline"];
    let shape = gtk::DropDown::from_strings(&["Block", "I-beam", "Underline"]);
    shape.set_selected(shapes.iter().position(|s| *s == initial.cursor_shape).unwrap_or(0) as u32);
    shape.connect_selected_notify({
        let change = change.clone();
        move |dropdown| {
            let value = shapes.get(dropdown.selected() as usize).copied().unwrap_or("block").to_string();
            change(&|s| s.cursor_shape = value.clone());
        }
    });
    row(&section, "Cursor shape", "Programs can still change it", &shape);
    switch_row(&section, "Blinking cursor", "", initial.cursor_blink, &change, |s, on| s.cursor_blink = on);

    // --- Window -------------------------------------------------------
    let section = new_section(&page, "Window");
    switch_row(
        &section,
        "Desktop notifications",
        "Post notifications to the desktop when you aren't looking at that terminal",
        initial.desktop_notifications,
        &change,
        |s, on| s.desktop_notifications = on,
    );
    switch_row(
        &section,
        "Branch and ports in the sidebar",
        "The git branch and listening ports under each workspace",
        initial.sidebar_details,
        &change,
        |s, on| s.sidebar_details = on,
    );
    switch_row(
        &section,
        "Pull request status",
        "The branch's GitHub pull request and its checks, from your own gh login",
        initial.pull_requests,
        &change,
        |s, on| s.pull_requests = on,
    );
    switch_row(
        &section,
        "Start the session automatically",
        "When the window opens and the session isn't running (after a reboot)",
        initial.auto_start,
        &change,
        |s, on| s.auto_start = on,
    );

    // --- Browser tabs -------------------------------------------------
    let section = new_section(&page, "Browser tabs");
    switch_row(
        &section,
        "Built-in browser provider",
        "Runs a headless browser for browser tabs; off if another provider serves the session",
        initial.browser_provider,
        &change,
        |s, on| s.browser_provider = on,
    );
    let installed: Vec<String> = cmux_browser_provider::installed_browsers().iter().map(|p| p.display().to_string()).collect();
    let mut choices = vec!["Automatic".to_string()];
    choices.extend(installed.iter().cloned());
    if !initial.browser.is_empty() && !installed.contains(&initial.browser) {
        choices.push(initial.browser.clone());
    }
    let browser = gtk::DropDown::from_strings(&choices.iter().map(String::as_str).collect::<Vec<_>>());
    browser.set_selected(choices.iter().position(|c| *c == initial.browser).unwrap_or(0) as u32);
    browser.connect_selected_notify({
        let (change, choices) = (change.clone(), choices.clone());
        move |dropdown| {
            let index = dropdown.selected() as usize;
            let value = if index == 0 { String::new() } else { choices.get(index).cloned().unwrap_or_default() };
            change(&|s| s.browser = value.clone());
        }
    });
    row(&section, "Browser", "Automatic prefers Chromium, then Brave and Edge, then Chrome", &browser);

    if context.cookie_sources.is_empty() {
        let none = gtk::Label::new(Some("No other browser profiles found to import cookies from."));
        none.add_css_class("dim-label");
        none.set_xalign(0.0);
        section.append(&none);
    } else {
        let sources = gtk::DropDown::from_strings(&context.cookie_sources.iter().map(String::as_str).collect::<Vec<_>>());
        let import = gtk::Button::with_label("Import");
        import.connect_clicked({
            let (host, sources) = (host.clone(), sources.clone());
            move |_| host(Request::ImportCookies(sources.selected() as usize))
        });
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.append(&sources);
        controls.append(&import);
        row(
            &section,
            "Import cookies",
            "Copies a browser profile's cookies into browser tabs so you start logged in. Needs a browser tab open; \
             your keyring may ask to be unlocked.",
            &controls,
        );
    }

    // --- Config files -------------------------------------------------
    let section = new_section(&page, "Config files");
    let note = gtk::Label::new(Some(
        "Keys, the TUI's sidebar and status bar, and the session's other settings are in cmux-tui.json. \
         Palette commands are in cmux.json.",
    ));
    note.set_wrap(true);
    note.set_xalign(0.0);
    note.add_css_class("dim-label");
    section.append(&note);
    for (label, file) in context.files {
        let button = gtk::Button::with_label("Open");
        button.connect_clicked({
            let (window, file) = (window.downgrade(), file.clone());
            move |_| {
                // A missing cmux.json starts out empty, ready to fill in.
                if !file.exists() && file.file_name().is_some_and(|n| n == "cmux.json") {
                    if let Some(dir) = file.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&file, "{\n  \"commands\": []\n}\n");
                }
                let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&file)));
                launcher.launch(window.upgrade().as_ref(), gio::Cancellable::NONE, |_| {});
            }
        });
        row(&section, &label, &file.display().to_string(), &button);
    }
    let reload = gtk::Button::with_label("Reload");
    reload.connect_clicked({
        let host = host.clone();
        move |_| host(Request::ReloadConfig)
    });
    row(&section, "Reload the session's config", "After editing cmux-tui.json by hand", &reload);

    let scroller = gtk::ScrolledWindow::builder().child(&page).hscrollbar_policy(gtk::PolicyType::Never).build();
    window.set_child(Some(&scroller));
    window
}

fn new_section(page: &gtk::Box, title: &str) -> gtk::Box {
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("heading");
    heading.set_xalign(0.0);
    heading.set_margin_top(12);
    page.append(&heading);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 10);
    rows.set_margin_start(6);
    page.append(&rows);
    rows
}

fn row(section: &gtk::Box, title: &str, detail: &str, control: &impl IsA<gtk::Widget>) {
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.append(&label);
    if !detail.is_empty() {
        let detail = gtk::Label::new(Some(detail));
        detail.set_xalign(0.0);
        detail.set_wrap(true);
        detail.add_css_class("dim-label");
        detail.add_css_class("caption");
        text.append(&detail);
    }
    control.set_valign(gtk::Align::Center);
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    line.append(&text);
    line.append(control);
    section.append(&line);
}

fn switch_row(
    section: &gtk::Box,
    title: &str,
    detail: &str,
    on: bool,
    change: &Rc<dyn Fn(&dyn Fn(&mut Settings))>,
    set: fn(&mut Settings, bool),
) {
    let switch = gtk::Switch::new();
    switch.set_active(on);
    switch.connect_active_notify({
        let change = change.clone();
        move |switch| {
            let on = switch.is_active();
            change(&|s| set(s, on));
        }
    });
    row(section, title, detail, &switch);
}
