//! The notification panel: every notification the session retains, newest
//! first, in a popover under the header bar's bell (like the macOS app's
//! notification panel).
//!
//! The bell shows how many are unread. A row shows the level mark, title,
//! body, where it came from and when; unread rows are bold. Activating a row
//! jumps to its tab (showing the tab marks it read, for every client).
//! **Mark all read** and **Clear all** act on the whole list. The host does
//! everything that touches the session, through [`Request`].

use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::backend::Note;

/// What the panel asks of its host.
pub enum Request {
    /// Jump to the tab with this `tab_…` id.
    Show(String),
    ReadAll,
    ClearAll,
}

pub type Host = Rc<dyn Fn(Request)>;

/// One row: the note, and where it is (`workspace · tab`) when its tab
/// still exists.
pub struct Row<'a> {
    pub note: &'a Note,
    pub place: Option<String>,
}

#[derive(Clone)]
pub struct NotesPanel {
    pub button: gtk::MenuButton,
    popover: gtk::Popover,
    count: gtk::Label,
    list: gtk::ListBox,
    read_all: gtk::Button,
    clear: gtk::Button,
    /// Tab id per row, in row order (`None`: its tab is gone).
    targets: Rc<std::cell::RefCell<Vec<Option<String>>>>,
}

impl NotesPanel {
    pub fn new(host: Host) -> Self {
        let icon = gtk::Image::from_icon_name("preferences-system-notifications-symbolic");
        let count = gtk::Label::new(None);
        count.add_css_class("notes-count");
        count.set_visible(false);
        let face = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        face.append(&icon);
        face.append(&count);

        let heading = gtk::Label::new(Some("Notifications"));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        heading.set_hexpand(true);
        let read_all = gtk::Button::with_label("Mark all read");
        read_all.set_tooltip_text(Some("Mark every notification read"));
        let clear = gtk::Button::with_label("Clear all");
        clear.set_tooltip_text(Some("Delete every notification in the session, for every window and the TUI"));
        for button in [&read_all, &clear] {
            button.add_css_class("flat");
        }
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bar.set_margin_start(6);
        bar.append(&heading);
        bar.append(&read_all);
        bar.append(&clear);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        list.add_css_class("navigation-sidebar");
        let placeholder = gtk::Label::new(Some("No notifications"));
        placeholder.add_css_class("dim-label");
        placeholder.set_margin_top(18);
        placeholder.set_margin_bottom(18);
        list.set_placeholder(Some(&placeholder));
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(480)
            .min_content_width(380)
            .build();

        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_width_request(400);
        body.append(&bar);
        body.append(&scroller);
        let popover = gtk::Popover::new();
        popover.set_child(Some(&body));

        let button = gtk::MenuButton::new();
        button.set_child(Some(&face));
        button.set_popover(Some(&popover));
        button.set_tooltip_text(Some("Notifications (Ctrl+Shift+I)"));

        let panel = Self {
            button,
            popover,
            count,
            list,
            read_all,
            clear,
            targets: Rc::new(std::cell::RefCell::new(Vec::new())),
        };
        panel.list.connect_row_activated({
            let (panel, host) = (panel.clone(), host.clone());
            move |_, row| {
                let target = panel.targets.borrow().get(row.index() as usize).cloned().flatten();
                if let Some(tab) = target {
                    panel.popover.popdown();
                    host(Request::Show(tab));
                }
            }
        });
        panel.read_all.connect_clicked({
            let host = host.clone();
            move |_| host(Request::ReadAll)
        });
        panel.clear.connect_clicked(move |_| host(Request::ClearAll));
        panel
    }

    /// The popover, for debug snapshots.
    pub fn popover(&self) -> gtk::Popover {
        self.popover.clone()
    }

    pub fn toggle(&self) {
        if self.popover.is_visible() {
            self.popover.popdown();
        } else {
            self.button.popup();
        }
    }

    /// Redraw the rows and the unread count.
    pub fn show(&self, rows: &[Row]) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let unread = rows.iter().filter(|r| r.note.unread).count();
        self.count.set_text(&unread.to_string());
        self.count.set_visible(unread > 0);
        self.button.set_tooltip_text(Some(&match unread {
            0 => "Notifications (Ctrl+Shift+I)".to_string(),
            n => format!("Notifications: {n} unread (Ctrl+Shift+I)"),
        }));
        self.read_all.set_sensitive(unread > 0);
        self.clear.set_sensitive(!rows.is_empty());
        let now = glib::real_time() / 1000;
        let mut targets = Vec::with_capacity(rows.len());
        for row in rows {
            self.list.append(&row_widget(row, now));
            targets.push(row.place.as_ref().and(row.note.tab_id.clone()));
        }
        *self.targets.borrow_mut() = targets;
    }
}

fn row_widget(row: &Row, now_ms: i64) -> gtk::Box {
    let note = row.note;
    let mark = gtk::Label::new(Some(if note.unread { "●" } else { "○" }));
    mark.add_css_class("att");
    mark.add_css_class(match note.level.as_str() {
        "error" => "att-error",
        "warning" => "att-warning",
        _ => "att-info",
    });
    mark.set_valign(gtk::Align::Start);

    let title = gtk::Label::new(Some(if note.title.is_empty() { "Notification" } else { &note.title }));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_hexpand(true);
    if note.unread {
        title.add_css_class("heading");
    }
    let when = gtk::Label::new(Some(&ago(note.created_at as i64, now_ms)));
    when.add_css_class("dim-label");
    when.add_css_class("caption");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    top.append(&title);
    top.append(&when);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.append(&top);
    if !note.body.is_empty() {
        let body = gtk::Label::new(Some(&note.body));
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        body.set_lines(3);
        body.set_ellipsize(gtk::pango::EllipsizeMode::End);
        body.set_max_width_chars(48);
        text.append(&body);
    }
    let place = gtk::Label::new(Some(row.place.as_deref().unwrap_or("tab closed")));
    place.set_xalign(0.0);
    place.set_ellipsize(gtk::pango::EllipsizeMode::End);
    place.add_css_class("dim-label");
    place.add_css_class("caption");
    text.append(&place);

    let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    widget.set_margin_top(4);
    widget.set_margin_bottom(4);
    widget.append(&mark);
    widget.append(&text);
    if row.place.is_none() {
        widget.set_tooltip_text(Some("Its tab has closed"));
    }
    widget
}

/// "just now", "5 min ago", "3 h ago", else the local date and time.
fn ago(then_ms: i64, now_ms: i64) -> String {
    let seconds = (now_ms - then_ms).max(0) / 1000;
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => glib::DateTime::from_unix_local(then_ms / 1000)
            .and_then(|t| t.format("%b %e %H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default(),
    }
}
