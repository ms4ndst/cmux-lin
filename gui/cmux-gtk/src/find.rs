//! Find in a terminal pane: a bar that slides in above the terminal
//! (Ctrl+Shift+F), searching the scrollback and screen with VTE's own
//! search.
//!
//! The search runs as you type, from the bottom up (the newest output
//! first). Enter or ↑ goes to the previous (older) match, Shift+Enter or ↓
//! to the next; both wrap around. **Aa** matches case, **.\*** treats the
//! text as a regular expression (otherwise it's literal). Escape closes the
//! bar and puts the focus back in the terminal. The field turns red when
//! nothing matches.

use gtk::glib;
use gtk::prelude::*;
use vte4::prelude::*;

/// PCRE2 compile flags VTE's search takes (`vte_regex_new_for_search`
/// requires `MULTILINE`).
const PCRE2_CASELESS: u32 = 0x0000_0008;
const PCRE2_MULTILINE: u32 = 0x0000_0400;

#[derive(Clone)]
pub struct FindBar {
    pub root: gtk::Revealer,
    entry: gtk::SearchEntry,
    case: gtk::ToggleButton,
    regex: gtk::ToggleButton,
    term: vte4::Terminal,
}

impl FindBar {
    pub fn new(term: &vte4::Terminal) -> Self {
        let entry = gtk::SearchEntry::new();
        entry.set_placeholder_text(Some("Find"));
        entry.set_hexpand(true);
        let case = gtk::ToggleButton::with_label("Aa");
        case.set_tooltip_text(Some("Match case"));
        let regex = gtk::ToggleButton::with_label(".*");
        regex.set_tooltip_text(Some("Regular expression"));
        let up = gtk::Button::from_icon_name("go-up-symbolic");
        up.set_tooltip_text(Some("Previous match (Enter)"));
        let down = gtk::Button::from_icon_name("go-down-symbolic");
        down.set_tooltip_text(Some("Next match (Shift+Enter)"));
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.set_tooltip_text(Some("Close (Escape)"));
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        bar.add_css_class("find-bar");
        bar.append(&entry);
        for button in [case.upcast_ref::<gtk::Widget>(), regex.upcast_ref(), up.upcast_ref(), down.upcast_ref(), close.upcast_ref()] {
            button.add_css_class("flat");
            bar.append(button);
        }
        let root = gtk::Revealer::new();
        root.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        root.set_child(Some(&bar));

        let find = Self { root, entry, case, regex, term: term.clone() };
        find.entry.connect_search_changed({
            let find = find.clone();
            move |_| find.apply()
        });
        for toggle in [&find.case, &find.regex] {
            toggle.connect_toggled({
                let find = find.clone();
                move |_| find.apply()
            });
        }
        find.entry.connect_activate({
            let find = find.clone();
            move |_| find.step(true)
        });
        // Ctrl+G / Ctrl+Shift+G, built into the search entry.
        find.entry.connect_next_match({
            let find = find.clone();
            move |_| find.step(false)
        });
        find.entry.connect_previous_match({
            let find = find.clone();
            move |_| find.step(true)
        });
        find.entry.connect_stop_search({
            let find = find.clone();
            move |_| find.close()
        });
        // Shift+Enter: the other direction (activate only sees Enter).
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let find = find.clone();
            move |_, key, _, state| {
                if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                    && state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                {
                    find.step(false);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        });
        find.entry.add_controller(keys);
        up.connect_clicked({
            let find = find.clone();
            move |_| find.step(true)
        });
        down.connect_clicked({
            let find = find.clone();
            move |_| find.step(false)
        });
        close.connect_clicked({
            let find = find.clone();
            move |_| find.close()
        });
        find
    }

    /// Show the bar (or focus it if shown), with its text selected.
    pub fn open(&self) {
        self.root.set_reveal_child(true);
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    /// Open with this text and search for it (scripting: `app.find-text`).
    pub fn open_with(&self, text: &str) {
        self.open();
        self.entry.set_text(text);
        self.apply(); // at once, not after the entry's typing delay
    }

    /// Hide the bar, clear the highlight, and return to the terminal.
    pub fn close(&self) {
        self.root.set_reveal_child(false);
        self.term.search_set_regex(None, 0);
        self.term.unselect_all();
        self.term.grab_focus();
    }

    /// The text, case or mode changed: search again from the bottom.
    fn apply(&self) {
        let text = self.entry.text();
        self.entry.remove_css_class("error");
        if text.is_empty() {
            self.term.search_set_regex(None, 0);
            self.term.unselect_all();
            return;
        }
        let pattern = if self.regex.is_active() { text.to_string() } else { glib::Regex::escape_string(text.as_str()).to_string() };
        let flags = PCRE2_MULTILINE | if self.case.is_active() { 0 } else { PCRE2_CASELESS };
        match vte4::Regex::for_search(&pattern, flags) {
            Ok(regex) => {
                self.term.search_set_regex(Some(&regex), 0);
                self.term.search_set_wrap_around(true);
                // Start from the newest output, not the last match.
                self.term.unselect_all();
                self.step(true);
            }
            Err(error) => {
                self.term.search_set_regex(None, 0);
                self.entry.add_css_class("error");
                self.entry.set_tooltip_text(Some(&format!("Invalid regular expression: {error}")));
            }
        }
    }

    /// Go to the previous (`up`, older) or next match.
    fn step(&self, up: bool) {
        if self.entry.text().is_empty() {
            return;
        }
        let found = if up { self.term.search_find_previous() } else { self.term.search_find_next() };
        if std::env::var_os("CMUX_GTK_DEBUG").is_some() {
            eprintln!("cmux-gtk: find {} found={found} selection={:?}", if up { "up" } else { "down" }, self.term.has_selection());
        }
        self.entry.set_tooltip_text(None);
        if found {
            self.entry.remove_css_class("error");
        } else {
            self.entry.add_css_class("error");
        }
    }
}
