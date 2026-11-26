//! The view for a browser tab: a URL bar plus the page, rendered from the
//! frames the session streams (the page itself runs in the session's browser
//! provider, so agents and the TUI see the same page).
//!
//! Copy and cut read the page's selection (the host does that over DevTools,
//! via `host`) and put it on the desktop clipboard; paste types the desktop
//! clipboard into the page. Selecting itself happens in the page: drag,
//! double/triple click, Shift+arrows, Ctrl+A.
//!
//! Right-click opens a native menu (navigation, clipboard, and for a link:
//! open in new tab / copy address); Shift+right-click goes to the page, for
//! sites with their own context menus. The headless page shows no menu of
//! its own.
//!
//! Input is forwarded as protocol browser commands through `send`. Pointer
//! input must quote the guard token of the frame it was aimed at
//! (`pointer_frame_seq`), so clicks can't land on a page that changed
//! underneath them; without a token, pointer input is dropped.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::{Value, json};

use crate::backend::Frame;

/// Sends one browser command (`cmd`, fields) for this pane's tab.
pub type Send = Rc<dyn Fn(&str, Value)>;

/// What the view asks of its host (things that need the session or a
/// DevTools round trip).
pub enum HostRequest {
    /// Copy the tab's selection to the clipboard; `cut` deletes it after.
    Copy { cut: bool },
    /// Open `url` in a new browser tab next to this one.
    OpenInNewTab(String),
    /// Find the link at page point (`page_x`, `page_y`), then call
    /// [`WebView::show_menu`] at widget point (`x`, `y`).
    ContextMenu { x: f64, y: f64, page_x: f64, page_y: f64 },
}

pub type Host = Rc<dyn Fn(HostRequest)>;

pub struct WebView {
    pub root: gtk::Box,
    entry: gtk::Entry,
    picture: gtk::Picture,
    status: gtk::Label,
    /// Shows the page, or `message` while there is no page to show.
    body: gtk::Stack,
    message: gtk::Label,
    /// Size of the shown frame in page pixels.
    frame: Rc<Cell<(f64, f64)>>,
    pointer_seq: Rc<Cell<Option<u64>>>,
    /// Mouse button held down, so drags select (moves carry the button).
    held: Rc<Cell<Option<&'static str>>>,
    host: Host,
    send: Send,
    /// The right-click menu, created once and re-filled per use.
    menu: gtk::PopoverMenu,
    /// The page's current URL (for "Open page in default browser").
    url: Rc<std::cell::RefCell<String>>,
    /// Last pointer position over the page (widget coordinates).
    pointer: Rc<Cell<Option<(f64, f64)>>>,
}

impl WebView {
    pub fn new(send: Send, host: Host) -> Self {
        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        back.set_tooltip_text(Some("Back"));
        let forward = gtk::Button::from_icon_name("go-next-symbolic");
        forward.set_tooltip_text(Some("Forward"));
        let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
        reload.set_tooltip_text(Some("Reload"));
        for button in [&back, &forward, &reload] {
            button.add_css_class("flat");
        }
        let entry = gtk::Entry::new();
        entry.set_hexpand(true);
        entry.set_placeholder_text(Some("Enter a URL"));
        let status = gtk::Label::new(None);
        status.add_css_class("dim-label");
        status.add_css_class("caption");
        status.set_margin_start(6);
        status.set_margin_end(6);

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        bar.add_css_class("browser-bar");
        bar.append(&back);
        bar.append(&forward);
        bar.append(&reload);
        bar.append(&entry);
        bar.append(&status);

        let picture = gtk::Picture::new();
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::ScaleDown);
        picture.set_focusable(true);
        picture.set_halign(gtk::Align::Fill);
        picture.set_valign(gtk::Align::Fill);

        let message = gtk::Label::new(Some("Starting browser…"));
        message.set_wrap(true);
        message.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        message.set_width_chars(20);
        message.set_justify(gtk::Justification::Center);
        message.set_max_width_chars(60);
        message.set_selectable(true);
        message.add_css_class("dim-label");
        message.set_margin_start(24);
        message.set_margin_end(24);
        let body = gtk::Stack::new();
        body.set_vexpand(true);
        body.add_named(&picture, Some("page"));
        body.add_named(&message, Some("message"));
        body.set_visible_child_name("message");

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&bar);
        root.append(&body);

        let view = Self {
            root,
            entry,
            picture,
            status,
            body,
            message,
            frame: Rc::new(Cell::new((0.0, 0.0))),
            pointer_seq: Rc::new(Cell::new(None)),
            held: Rc::new(Cell::new(None)),
            host: host.clone(),
            send: send.clone(),
            menu: gtk::PopoverMenu::from_model(None::<&gio::MenuModel>),
            url: Rc::new(std::cell::RefCell::new(String::new())),
            pointer: Rc::new(Cell::new(None)),
        };
        view.menu.set_parent(&view.picture);
        view.menu.set_has_arrow(false);
        view.menu.set_halign(gtk::Align::Start);
        view.install_menu_actions();

        for (button, cmd) in [(&back, "browser-back"), (&forward, "browser-forward"), (&reload, "browser-reload")] {
            let send = send.clone();
            button.connect_clicked(move |_| send(cmd, json!({})));
        }
        view.entry.connect_activate({
            let send = send.clone();
            let picture = view.picture.clone();
            move |entry| {
                let url = normalize_url(&entry.text());
                if !url.is_empty() {
                    send("browser-navigate", json!({ "url": url }));
                    picture.grab_focus();
                }
            }
        });
        view.wire_pointer(&send);
        view.wire_keys(&send, host);
        view
    }

    /// Show the tab's state (URL, loading status, errors). `problem` is why
    /// the built-in provider can't start a browser, if it can't.
    pub fn show_state(&self, url: &str, status: &str, error: Option<&str>, problem: Option<&str>) {
        if !url.is_empty() {
            *self.url.borrow_mut() = url.to_string();
        }
        if !self.entry.has_focus() && self.entry.text() != url {
            self.entry.set_text(url);
        }
        // Until the page has a frame, say why rather than show a blank area.
        if self.frame.get().0 <= 0.0 || status != "live" {
            let text = match (status, problem, error) {
                ("live", _, _) => "Loading…".to_string(),
                (_, Some(problem), _) => problem.to_string(),
                ("failed", _, Some(error)) => format!("The page failed to load.\n\n{error}"),
                ("failed", _, None) => "The page failed to load.".to_string(),
                _ => "Starting browser…".to_string(),
            };
            self.message.set_text(&text);
            self.body.set_visible_child_name("message");
        }
        let text = match (status, error) {
            (_, Some(error)) if !error.is_empty() => format!("error: {error}"),
            ("live", _) => String::new(),
            (other, _) => other.to_string(),
        };
        self.status.set_text(&text);
        self.status.set_tooltip_text(error);
    }

    /// Update the message of a tab that has no page yet with the provider's
    /// problem (or the default once it's resolved).
    pub fn show_problem(&self, problem: Option<&str>) {
        if self.frame.get().0 <= 0.0 {
            self.message.set_text(problem.unwrap_or("Starting browser…"));
            self.body.set_visible_child_name("message");
        }
    }

    /// Show a frame. Returns its pointer guard token, which the caller
    /// acknowledges with `browser-frame-presented` now that it's on screen.
    pub fn show_frame(&self, frame: Frame) -> Option<u64> {
        let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(frame.png)).ok()?;
        self.picture.set_paintable(Some(&texture));
        self.body.set_visible_child_name("page");
        self.frame.set((f64::from(frame.width), f64::from(frame.height)));
        self.pointer_seq.set(frame.pointer_seq);
        frame.pointer_seq
    }

    pub fn clear(&self) {
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.frame.set((0.0, 0.0));
        self.pointer_seq.set(None);
        self.entry.set_text("");
        self.status.set_text("connecting…");
        self.message.set_text("Starting browser…");
        self.body.set_visible_child_name("message");
    }

    /// The page area as a cell grid, for the server's sizing.
    pub fn grid(&self, (cell_w, cell_h): (i32, i32)) -> (u16, u16) {
        let (w, h) = (self.picture.width(), self.picture.height());
        if cell_w <= 0 || cell_h <= 0 || w <= 0 || h <= 0 {
            return (0, 0);
        }
        ((w / cell_w).clamp(1, 10_000) as u16, (h / cell_h).clamp(1, 10_000) as u16)
    }

    /// Copy and paste entry points for menus and actions:
    /// (copy/cut, paste).
    pub fn clip_handle(&self) -> (Rc<dyn Fn(bool)>, Rc<dyn Fn()>) {
        let (picture, send, host) = (self.picture.clone(), self.send.clone(), self.host.clone());
        (Rc::new(move |cut| host(HostRequest::Copy { cut })), Rc::new(move || paste(&picture, &send)))
    }

    /// Actions behind the right-click menu, as the `web.` group on the page.
    fn install_menu_actions(&self) {
        let group = gio::SimpleActionGroup::new();
        let add = |name: &str, parameter: Option<&glib::VariantTy>, run: Box<dyn Fn(Option<String>)>| {
            let action = gio::SimpleAction::new(name, parameter);
            action.connect_activate(move |_, value| run(value.and_then(|v| v.get::<String>())));
            group.add_action(&action);
        };
        for (name, cmd) in [("back", "browser-back"), ("forward", "browser-forward"), ("reload", "browser-reload")] {
            let send = self.send.clone();
            add(name, None, Box::new(move |_| send(cmd, json!({}))));
        }
        let host = self.host.clone();
        add("copy", None, Box::new(move |_| host(HostRequest::Copy { cut: false })));
        let (picture, send) = (self.picture.clone(), self.send.clone());
        add("paste", None, Box::new(move |_| paste(&picture, &send)));
        let send = self.send.clone();
        add("select-all", None, Box::new(move |_| {
            send("browser-key-press", json!({
                "key": "a", "code": "KeyA", "modifiers": 2, "windows_virtual_key_code": 65,
            }));
        }));
        let host = self.host.clone();
        add("open-link", Some(glib::VariantTy::STRING), Box::new(move |url| {
            if let Some(url) = url {
                host(HostRequest::OpenInNewTab(url));
            }
        }));
        let picture = self.picture.clone();
        add("copy-link", Some(glib::VariantTy::STRING), Box::new(move |url| {
            if let Some(url) = url {
                picture.clipboard().set_text(&url);
            }
        }));
        let current = self.url.clone();
        add("open-external", None, Box::new(move |_| {
            let url = current.borrow().clone();
            if url.starts_with("http://") || url.starts_with("https://") {
                let _ = gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>);
            }
        }));
        self.picture.insert_action_group("web", Some(&group));
    }

    /// Opens the context menu without a click (Menu key, Shift+F10, or the
    /// `app.browser-menu` action): at the last pointer position, else the
    /// centre of the page. Call it after releasing any borrow of the view.
    pub fn opener(&self) -> impl Fn() + 'static {
        let (picture, frame, pointer, host) =
            (self.picture.clone(), self.frame.clone(), self.pointer.clone(), self.host.clone());
        move || {
            let (w, h) = (f64::from(picture.width()), f64::from(picture.height()));
            let (x, y) = pointer.get().unwrap_or((w / 2.0, h / 2.0));
            if let Some((page_x, page_y)) = map_to_page(&picture, frame.get(), x, y) {
                host(HostRequest::ContextMenu { x, y, page_x, page_y });
            }
        }
    }

    /// A handle that shows the right-click menu, usable after the caller has
    /// released any borrow of the view (popping up moves focus, which runs
    /// focus handlers).
    pub fn menu_shower(&self) -> impl Fn(f64, f64, Option<String>) + 'static {
        let menu = self.menu.clone();
        move |x, y, link| show_menu(&menu, x, y, link)
    }

    /// The widget that takes keyboard focus for this view.
    pub fn focus_widget(&self) -> gtk::Widget {
        self.picture.clone().upcast()
    }

    /// Mouse buttons, motion and wheel, mapped from widget to page pixels.
    fn wire_pointer(&self, send: &Send) {
        let to_page = {
            let picture = self.picture.clone();
            let frame = self.frame.clone();
            move |x: f64, y: f64| map_to_page(&picture, frame.get(), x, y)
        };

        let click = gtk::GestureClick::new();
        click.set_button(0);
        fn button_name(button: u32) -> &'static str {
            match button {
                2 => "middle",
                3 => "right",
                _ => "left",
            }
        }
        fn shift_held(gesture: &gtk::GestureClick) -> bool {
            gesture.current_event_state().contains(gdk::ModifierType::SHIFT_MASK)
        }
        click.connect_pressed({
            let (send, to_page, seq, picture, held, host) = (
                send.clone(),
                to_page.clone(),
                self.pointer_seq.clone(),
                self.picture.clone(),
                self.held.clone(),
                self.host.clone(),
            );
            move |gesture, presses, x, y| {
                picture.grab_focus();
                let (Some((px, py)), Some(frame_seq)) = (to_page(x, y), seq.get()) else { return };
                // Right-click opens our menu; Shift+right-click goes to the page.
                if gesture.current_button() == 3 && !shift_held(gesture) {
                    host(HostRequest::ContextMenu { x, y, page_x: px, page_y: py });
                    return;
                }
                held.set(Some(button_name(gesture.current_button())));
                send("browser-mouse-guarded", json!({
                    "kind": "down", "button": button_name(gesture.current_button()),
                    "click_count": presses, "frame_seq": frame_seq, "x_px": px, "y_px": py,
                }));
            }
        });
        click.connect_released({
            let (send, to_page, seq, held) =
                (send.clone(), to_page.clone(), self.pointer_seq.clone(), self.held.clone());
            move |gesture, presses, x, y| {
                held.set(None);
                if gesture.current_button() == 3 && !shift_held(gesture) {
                    return; // the press opened our menu
                }
                let (Some((px, py)), Some(frame_seq)) = (to_page(x, y), seq.get()) else { return };
                send("browser-mouse-guarded", json!({
                    "kind": "up", "button": button_name(gesture.current_button()),
                    "click_count": presses, "frame_seq": frame_seq, "x_px": px, "y_px": py,
                }));
            }
        });
        self.picture.add_controller(click);

        // Hover and drag, at most every 30 ms.
        let motion = gtk::EventControllerMotion::new();
        let last = Rc::new(Cell::new(Instant::now()));
        motion.connect_motion({
            let (send, to_page, seq, held) =
                (send.clone(), to_page.clone(), self.pointer_seq.clone(), self.held.clone());
            move |_, x, y| {
                if last.get().elapsed() < Duration::from_millis(30) {
                    return;
                }
                let (Some((px, py)), Some(frame_seq)) = (to_page(x, y), seq.get()) else { return };
                last.set(Instant::now());
                let mut fields = json!({ "kind": "move", "frame_seq": frame_seq, "x_px": px, "y_px": py });
                // While a button is held the move is a drag (text selection).
                if let Some(button) = held.get() {
                    fields["button"] = json!(button);
                }
                send("browser-mouse-guarded", fields);
            }
        });
        self.picture.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let pointer = Rc::new(Cell::new((0.0, 0.0)));
        let track = gtk::EventControllerMotion::new();
        track.connect_motion({
            let (pointer, last_pointer) = (pointer.clone(), self.pointer.clone());
            move |_, x, y| {
                pointer.set((x, y));
                last_pointer.set(Some((x, y)));
            }
        });
        self.picture.add_controller(track);
        scroll.connect_scroll({
            let (send, seq) = (send.clone(), self.pointer_seq.clone());
            move |_, _, dy| {
                let (x, y) = pointer.get();
                let (Some((px, py)), Some(frame_seq)) = (to_page(x, y), seq.get()) else {
                    return glib::Propagation::Proceed;
                };
                send("browser-wheel-guarded", json!({
                    "delta_y_px": dy * 60.0, "frame_seq": frame_seq, "x_px": px, "y_px": py,
                }));
                glib::Propagation::Stop
            }
        });
        self.picture.add_controller(scroll);
    }

    /// Keyboard: text goes in as inserted text, editing and navigation keys
    /// (and Ctrl/Alt chords) as key presses.
    fn wire_keys(&self, send: &Send, host: Host) {
        let keys = gtk::EventControllerKey::new();
        let send = send.clone();
        let picture = self.picture.clone();
        let open_menu = self.opener();
        keys.connect_key_pressed(move |_, key, _, state| {
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            let lower = key.to_lower();
            // Clipboard: Ctrl+C / Ctrl+Shift+C copy, Ctrl+X cut, Ctrl+V /
            // Ctrl+Shift+V / Shift+Insert paste. The page's own clipboard is
            // the headless browser's, so these go through the desktop's.
            if ctrl && lower == gdk::Key::c {
                host(HostRequest::Copy { cut: false });
                return glib::Propagation::Stop;
            }
            if ctrl && !shift && lower == gdk::Key::x {
                host(HostRequest::Copy { cut: true });
                return glib::Propagation::Stop;
            }
            if (ctrl && lower == gdk::Key::v) || (shift && key == gdk::Key::Insert) {
                paste(&picture, &send);
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::Menu || (shift && key == gdk::Key::F10) {
                open_menu();
                return glib::Propagation::Stop;
            }
            let mut modifiers = 0u32;
            if state.contains(gdk::ModifierType::ALT_MASK) {
                modifiers |= 1;
            }
            if state.contains(gdk::ModifierType::CONTROL_MASK) {
                modifiers |= 2;
            }
            if state.contains(gdk::ModifierType::SUPER_MASK) {
                modifiers |= 4;
            }
            if state.contains(gdk::ModifierType::SHIFT_MASK) {
                modifiers |= 8;
            }
            if let Some((name, vk, text)) = special_key(key) {
                let mut fields = json!({
                    "key": name, "code": name, "modifiers": modifiers, "windows_virtual_key_code": vk,
                });
                if let Some(text) = text {
                    fields["text"] = json!(text);
                }
                send("browser-key-press", fields);
                return glib::Propagation::Stop;
            }
            let Some(ch) = key.to_unicode().filter(|c| !c.is_control()) else {
                return glib::Propagation::Proceed;
            };
            if modifiers & 0b0111 != 0 && ch.is_ascii_alphanumeric() {
                // Ctrl/Alt/Super chord, e.g. Ctrl+A or Ctrl+L.
                let upper = ch.to_ascii_uppercase();
                let code = if ch.is_ascii_digit() { format!("Digit{ch}") } else { format!("Key{upper}") };
                send("browser-key-press", json!({
                    "key": ch.to_ascii_lowercase().to_string(), "code": code,
                    "modifiers": modifiers, "windows_virtual_key_code": upper as u32,
                }));
                return glib::Propagation::Stop;
            }
            send("browser-insert-text", json!({ "text": ch.to_string() }));
            glib::Propagation::Stop
        });
        self.picture.add_controller(keys);
    }
}

/// Widget point → page pixel for a frame of size `frame` shown ScaleDown
/// (never enlarged, centred). `None` outside the page.
fn map_to_page(picture: &gtk::Picture, frame: (f64, f64), x: f64, y: f64) -> Option<(f64, f64)> {
    let (fw, fh) = frame;
    let (w, h) = (f64::from(picture.width()), f64::from(picture.height()));
    if fw <= 0.0 || fh <= 0.0 || w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = (w / fw).min(h / fh).min(1.0);
    let (ox, oy) = ((w - fw * scale) / 2.0, (h - fh * scale) / 2.0);
    let (px, py) = ((x - ox) / scale, (y - oy) / scale);
    (px >= 0.0 && py >= 0.0 && px <= fw && py <= fh).then_some((px, py))
}

/// Type the desktop clipboard's text into the page.
fn paste(widget: &gtk::Picture, send: &Send) {
    let clipboard = widget.clipboard();
    let send = send.clone();
    glib::spawn_future_local(async move {
        if let Ok(Some(text)) = clipboard.read_text_future().await {
            if !text.is_empty() {
                send("browser-insert-text", json!({ "text": text.as_str() }));
            }
        }
    });
}

/// Keys sent as key presses: (DOM key/code name, Windows virtual key code,
/// text the key produces).
fn special_key(key: gdk::Key) -> Option<(&'static str, u32, Option<&'static str>)> {
    Some(match key {
        gdk::Key::Return | gdk::Key::KP_Enter => ("Enter", 13, Some("\r")),
        gdk::Key::BackSpace => ("Backspace", 8, None),
        gdk::Key::Tab => ("Tab", 9, None),
        gdk::Key::Escape => ("Escape", 27, None),
        gdk::Key::Left => ("ArrowLeft", 37, None),
        gdk::Key::Up => ("ArrowUp", 38, None),
        gdk::Key::Right => ("ArrowRight", 39, None),
        gdk::Key::Down => ("ArrowDown", 40, None),
        gdk::Key::Home => ("Home", 36, None),
        gdk::Key::End => ("End", 35, None),
        gdk::Key::Page_Up => ("PageUp", 33, None),
        gdk::Key::Page_Down => ("PageDown", 34, None),
        gdk::Key::Delete => ("Delete", 46, None),
        _ => return None,
    })
}

/// Bare domains get `https://`; anything with a scheme or a known prefix
/// passes through (the same rule the TUI's prompt uses).
fn normalize_url(input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return String::new();
    }
    let passthrough = ["about:", "file:", "data:", "chrome:", "devtools:"];
    if input.contains("://") || passthrough.iter().any(|p| input.starts_with(p)) {
        input.to_string()
    } else {
        format!("https://{input}")
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        // A popover must be detached before its parent widget goes away.
        self.menu.unparent();
    }
}

/// Fill and pop up the right-click menu at widget point (`x`, `y`); `link`
/// adds the link items.
fn show_menu(menu: &gtk::PopoverMenu, x: f64, y: f64, link: Option<String>) {
    let model = gio::Menu::new();
    if let Some(link) = link {
        let section = gio::Menu::new();
        let open = gio::MenuItem::new(Some("Open link in new tab"), None);
        open.set_action_and_target_value(Some("web.open-link"), Some(&link.to_variant()));
        section.append_item(&open);
        let copy = gio::MenuItem::new(Some("Copy link address"), None);
        copy.set_action_and_target_value(Some("web.copy-link"), Some(&link.to_variant()));
        section.append_item(&copy);
        model.append_section(None, &section);
    }
    let navigation = gio::Menu::new();
    navigation.append(Some("Back"), Some("web.back"));
    navigation.append(Some("Forward"), Some("web.forward"));
    navigation.append(Some("Reload"), Some("web.reload"));
    model.append_section(None, &navigation);
    let edit = gio::Menu::new();
    edit.append(Some("Copy"), Some("web.copy"));
    edit.append(Some("Paste"), Some("web.paste"));
    edit.append(Some("Select all"), Some("web.select-all"));
    model.append_section(None, &edit);
    let page = gio::Menu::new();
    page.append(Some("Open page in default browser"), Some("web.open-external"));
    model.append_section(None, &page);

    if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
        eprintln!("cmux-gtk: context menu at ({x:.0}, {y:.0}), {} sections", model.n_items());
    }
    menu.set_menu_model(Some(&model));
    menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    menu.popup();
}
