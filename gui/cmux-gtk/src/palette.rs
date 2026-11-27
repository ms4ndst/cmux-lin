//! The command palette (Ctrl+Shift+P): a search field over everything the
//! window can do, the session's workspaces and tabs, and custom commands.
//!
//! The host builds the items each time the palette opens, so workspaces,
//! tabs and `cmux.json` are always current. Typing filters them: every word
//! must appear in the label, its detail or its keywords, in order or
//! scattered; matches at the start of the label or of a word rank first.
//! Up/Down (or Ctrl+N/P) move, Enter or a click runs, Escape closes. The
//! focus goes back to where it was before the item runs, so a command typed
//! into "the focused terminal" lands there.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

/// One palette entry.
pub struct Item {
    pub label: String,
    /// Secondary text (what it does, where it is).
    pub detail: String,
    /// Extra words it should match.
    pub keywords: String,
    /// The key that does the same, shown on the right.
    pub shortcut: Option<String>,
    pub run: Rc<dyn Fn()>,
}

impl Item {
    pub fn new(label: impl Into<String>, run: impl Fn() + 'static) -> Self {
        Self { label: label.into(), detail: String::new(), keywords: String::new(), shortcut: None, run: Rc::new(run) }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn keywords(mut self, keywords: impl Into<String>) -> Self {
        self.keywords = keywords.into();
        self
    }

    pub fn shortcut(mut self, shortcut: &str) -> Self {
        self.shortcut = Some(shortcut.to_string());
        self
    }
}

/// Most rows drawn at once (the rest are a few keystrokes away).
const MAX_ROWS: usize = 200;

#[derive(Clone)]
pub struct Palette {
    pub root: gtk::Box,
    entry: gtk::SearchEntry,
    list: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    items: Rc<RefCell<Vec<Item>>>,
    /// Item index per row, in row order.
    shown: Rc<RefCell<Vec<usize>>>,
    /// Where the focus was when the palette opened.
    previous: Rc<RefCell<Option<gtk::Widget>>>,
}

impl Palette {
    pub fn new() -> Self {
        let entry = gtk::SearchEntry::new();
        entry.set_placeholder_text(Some("Type a command, workspace or tab"));
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Browse);
        list.set_activate_on_single_click(true);
        list.add_css_class("navigation-sidebar");
        let placeholder = gtk::Label::new(Some("Nothing matches"));
        placeholder.add_css_class("dim-label");
        placeholder.set_margin_top(12);
        placeholder.set_margin_bottom(12);
        list.set_placeholder(Some(&placeholder));
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(420)
            .build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.add_css_class("palette");
        root.set_halign(gtk::Align::Center);
        root.set_valign(gtk::Align::Start);
        root.set_margin_top(48);
        root.set_width_request(560);
        root.append(&entry);
        root.append(&scroller);
        root.set_visible(false);

        let palette = Self {
            root,
            entry,
            list,
            scroller: scroller.clone(),
            items: Rc::new(RefCell::new(Vec::new())),
            shown: Rc::new(RefCell::new(Vec::new())),
            previous: Rc::new(RefCell::new(None)),
        };
        palette.entry.connect_search_changed({
            let palette = palette.clone();
            move |_| palette.filter()
        });
        palette.entry.connect_activate({
            let palette = palette.clone();
            move |_| palette.run_selected()
        });
        palette.entry.connect_stop_search({
            let palette = palette.clone();
            move |_| palette.close()
        });
        palette.list.connect_row_activated({
            let palette = palette.clone();
            move |_, row| {
                palette.list.select_row(Some(row));
                palette.run_selected();
            }
        });
        // Up/Down and Ctrl+N/P move the selection while typing.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let palette = palette.clone();
            move |_, key, _, state| {
                let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                let step = match key {
                    gtk::gdk::Key::Down => 1,
                    gtk::gdk::Key::Up => -1,
                    gtk::gdk::Key::n if ctrl => 1,
                    gtk::gdk::Key::p if ctrl => -1,
                    gtk::gdk::Key::Page_Down => 8,
                    gtk::gdk::Key::Page_Up => -8,
                    gtk::gdk::Key::Escape => {
                        palette.close();
                        return glib::Propagation::Stop;
                    }
                    _ => return glib::Propagation::Proceed,
                };
                palette.move_selection(step);
                glib::Propagation::Stop
            }
        });
        palette.entry.add_controller(keys);
        // Leaving the palette (clicking elsewhere) closes it.
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave({
            let palette = palette.clone();
            move |_| {
                let palette = palette.clone();
                // After the focus has settled: moving to the list is fine.
                glib::idle_add_local_once(move || {
                    if palette.root.is_visible() && !palette.root.has_focus() && !has_focus_within(&palette.root) {
                        palette.close_without_refocus();
                    }
                });
            }
        });
        palette.root.add_controller(focus);
        palette
    }

    pub fn is_open(&self) -> bool {
        self.root.is_visible()
    }

    /// Show the palette with these items (or close it if it's open).
    pub fn toggle(&self, items: Vec<Item>, focus_now: Option<gtk::Widget>) {
        if self.is_open() {
            self.close();
            return;
        }
        *self.items.borrow_mut() = items;
        *self.previous.borrow_mut() = focus_now;
        self.entry.set_text("");
        self.root.set_visible(true);
        self.filter();
        self.entry.grab_focus();
    }

    /// Open with a query already typed (scripting: `app.palette-text`).
    pub fn open_with(&self, items: Vec<Item>, focus_now: Option<gtk::Widget>, query: &str) {
        if self.is_open() {
            self.close_without_refocus();
        }
        self.toggle(items, focus_now);
        self.entry.set_text(query);
        self.filter();
    }

    /// Close and give the focus back.
    pub fn close(&self) {
        self.close_without_refocus();
        let previous = self.previous.borrow_mut().take();
        if let Some(widget) = previous {
            widget.grab_focus();
        }
    }

    fn close_without_refocus(&self) {
        self.root.set_visible(false);
        self.items.borrow_mut().clear();
        self.shown.borrow_mut().clear();
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
    }

    fn filter(&self) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let query = self.entry.text().to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let items = self.items.borrow();
        let mut ranked: Vec<(i64, usize)> = items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| score(&terms, item).map(|s| (s, index)))
            .collect();
        // Best first; the host's order breaks ties.
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        ranked.truncate(MAX_ROWS);
        for (_, index) in &ranked {
            self.list.append(&row_widget(&items[*index]));
        }
        *self.shown.borrow_mut() = ranked.into_iter().map(|(_, index)| index).collect();
        if let Some(first) = self.list.row_at_index(0) {
            self.list.select_row(Some(&first));
        }
    }

    fn move_selection(&self, step: i32) {
        let count = self.shown.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let current = self.list.selected_row().map_or(0, |r| r.index());
        let next = (current + step).clamp(0, count - 1);
        if let Some(row) = self.list.row_at_index(next) {
            self.list.select_row(Some(&row));
            // Keep it in view; the focus stays in the field.
            if let Some(bounds) = row.compute_bounds(&self.list) {
                let adjustment = self.scroller.vadjustment();
                let (top, bottom) = (f64::from(bounds.y()), f64::from(bounds.y() + bounds.height()));
                if top < adjustment.value() {
                    adjustment.set_value(top);
                } else if bottom > adjustment.value() + adjustment.page_size() {
                    adjustment.set_value(bottom - adjustment.page_size());
                }
            }
        }
    }

    fn run_selected(&self) {
        let index = self.list.selected_row().and_then(|row| self.shown.borrow().get(row.index() as usize).copied());
        let run = index.and_then(|i| self.items.borrow().get(i).map(|item| item.run.clone()));
        let Some(run) = run else { return };
        self.close();
        run();
    }
}

/// Run the best match for `query` among `items` without showing anything
/// (scripting: `app.palette-run`). Returns its label.
pub fn run_best(items: Vec<Item>, query: &str) -> Option<String> {
    let query = query.to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();
    let best = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| score(&terms, item).map(|s| (s, index)))
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))?;
    let item = &items[best.1];
    (item.run)();
    Some(item.label.clone())
}

fn has_focus_within(widget: &gtk::Box) -> bool {
    widget.root().and_then(|root| root.focus()).is_some_and(|focus| focus.is_ancestor(widget))
}

/// How well `item` matches every term, or `None`. A term at the start of
/// the label scores most, then at a word start, then anywhere in the label,
/// then in the detail or keywords, then as scattered letters of the label.
fn score(terms: &[&str], item: &Item) -> Option<i64> {
    if terms.is_empty() {
        return Some(0);
    }
    let label = item.label.to_lowercase();
    let rest = format!("{} {}", item.detail, item.keywords).to_lowercase();
    let mut total = 0;
    for term in terms {
        total += if let Some(at) = label.find(term) {
            let word_start = at == 0 || label[..at].ends_with([' ', ':', '·', '-', '/', '(']);
            if at == 0 {
                3000
            } else if word_start {
                2000 - at as i64
            } else {
                1000 - at as i64
            }
        } else if rest.contains(term) {
            500
        } else if let Some(gaps) = scattered(&label, term) {
            100 - gaps.min(99)
        } else {
            return None;
        };
    }
    Some(total)
}

/// The letters of `term` in order within `text`: how many letters lie
/// between them, or `None`.
fn scattered(text: &str, term: &str) -> Option<i64> {
    let mut chars = text.chars();
    let mut gaps = 0;
    let mut started = false;
    for wanted in term.chars() {
        loop {
            let c = chars.next()?;
            if c == wanted {
                started = true;
                break;
            }
            if started {
                gaps += 1;
            }
        }
    }
    Some(gaps)
}

fn row_widget(item: &Item) -> gtk::Box {
    let label = gtk::Label::new(Some(&item.label));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.set_hexpand(true);
    text.append(&label);
    if !item.detail.is_empty() {
        let detail = gtk::Label::new(Some(&item.detail));
        detail.set_xalign(0.0);
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.add_css_class("dim-label");
        detail.add_css_class("caption");
        text.append(&detail);
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_margin_top(2);
    row.set_margin_bottom(2);
    row.set_margin_start(4);
    row.set_margin_end(4);
    row.append(&text);
    if let Some(shortcut) = &item.shortcut {
        let keys = gtk::Label::new(Some(shortcut));
        keys.add_css_class("dim-label");
        keys.add_css_class("caption");
        keys.set_valign(gtk::Align::Center);
        row.append(&keys);
    }
    row
}
