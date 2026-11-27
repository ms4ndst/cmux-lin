//! Workspace layout commands from `cmux.json`: a command with a `workspace`
//! object (instead of a `command` string) opens a new workspace arranged as
//! a tree of splits whose leaves are panes of terminal and browser tabs, in
//! the macOS app's format:
//!
//! ```json
//! { "name": "API Stack", "restart": "confirm",
//!   "workspace": { "name": "API", "cwd": "./services/api", "env": { "PORT": "8080" },
//!     "setup": "nvm use",
//!     "layout": { "direction": "vertical", "split": 0.65, "children": [
//!       { "pane": { "surfaces": [ { "type": "terminal", "name": "Server", "command": "go run .", "focus": true } ] } },
//!       { "pane": { "surfaces": [ { "type": "browser", "name": "Health", "url": "http://localhost:8080" } ] } } ] } } }
//! ```
//!
//! `horizontal` splits put the children side by side, `vertical` ones one
//! above the other; `split` is the first child's share (0.1–0.9, default
//! 0.5). The workspace is built step by step over its own connection
//! (`new-workspace`, `split`, `new-tab`, `new-browser-tab`,
//! `rename-surface`, `set-split-ratio`). The protocol has no per-terminal
//! environment or start directory for split panes, so each terminal gets a
//! short prefix typed into its shell first (` cd … && export … && clear`;
//! the leading space keeps it out of shell history), then `setup` (first
//! terminal only), then its `command`. The workspace `color` isn't used:
//! the session has no workspace colours.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::backend::UiMsg;

/// What to do when a workspace with the same name exists (`restart`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Restart {
    /// Make another one (the default).
    New,
    /// Ask, then replace it.
    Confirm,
    /// Replace it without asking.
    Recreate,
    /// Switch to the existing one.
    Ignore,
}

#[derive(Debug, Clone)]
pub struct Surface {
    pub browser: bool,
    pub name: Option<String>,
    pub focus: bool,
    pub command: Option<String>,
    pub url: Option<String>,
    /// Resolved against the workspace directory.
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub enum Node {
    /// `right`: side by side; otherwise one above the other.
    Split { right: bool, ratio: f64, first: Box<Node>, second: Box<Node> },
    Pane(Vec<Surface>),
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub name: String,
    pub setup: Option<String>,
    pub layout: Node,
    pub restart: Restart,
}

/// Parse a command's `workspace` object. `base` is the directory relative
/// paths start from (the project folder for a project `cmux.json`).
pub fn parse(command_name: &str, entry: &Value, base: &Path) -> Result<Spec, String> {
    let ws = entry.get("workspace").filter(|w| w.is_object()).ok_or("no workspace object")?;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    let ws_dir = resolve(ws.get("cwd").and_then(Value::as_str), base, &home);
    let ws_env = env_of(ws.get("env"));
    let restart = match entry.get("restart").and_then(Value::as_str) {
        None | Some("new") => Restart::New,
        Some("confirm") => Restart::Confirm,
        Some("recreate") => Restart::Recreate,
        Some("ignore") => Restart::Ignore,
        Some(other) => return Err(format!("unknown restart \"{other}\"")),
    };
    let layout = match ws.get("layout") {
        Some(node) => parse_node(node, &ws_dir, &ws_env, &home)?,
        // No layout: one terminal in the workspace directory.
        None => Node::Pane(vec![Surface {
            browser: false,
            name: None,
            focus: true,
            command: None,
            url: None,
            cwd: ws_dir.clone(),
            env: ws_env.clone(),
        }]),
    };
    let name = ws
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(command_name)
        .to_string();
    let setup = ws.get("setup").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string);
    Ok(Spec { name, setup, layout, restart })
}

fn parse_node(node: &Value, ws_dir: &Path, ws_env: &[(String, String)], home: &Path) -> Result<Node, String> {
    if let Some(pane) = node.get("pane") {
        let surfaces = pane.get("surfaces").and_then(Value::as_array).filter(|s| !s.is_empty());
        let surfaces = surfaces.ok_or("a pane needs at least one surface")?;
        let surfaces = surfaces
            .iter()
            .map(|s| {
                let browser = match s.get("type").and_then(Value::as_str) {
                    None | Some("terminal") => false,
                    Some("browser") => true,
                    Some(other) => return Err(format!("unknown surface type \"{other}\"")),
                };
                let text = |key: &str| s.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
                let url = text("url");
                if browser && url.is_none() {
                    return Err("a browser surface needs a url".to_string());
                }
                let mut env = ws_env.to_vec();
                for (key, value) in env_of(s.get("env")) {
                    env.retain(|(k, _)| *k != key);
                    env.push((key, value));
                }
                Ok(Surface {
                    browser,
                    name: text("name"),
                    focus: s.get("focus").and_then(Value::as_bool).unwrap_or(false),
                    command: text("command"),
                    url,
                    cwd: resolve(s.get("cwd").and_then(Value::as_str), ws_dir, home),
                    env,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        return Ok(Node::Pane(surfaces));
    }
    let children = node.get("children").and_then(Value::as_array).ok_or("a node needs a pane or children")?;
    let [first, second] = children.as_slice() else { return Err("a split needs exactly two children".to_string()) };
    let right = match node.get("direction").and_then(Value::as_str) {
        Some("horizontal") => true,
        Some("vertical") => false,
        _ => return Err("a split needs direction \"horizontal\" or \"vertical\"".to_string()),
    };
    let ratio = node.get("split").and_then(Value::as_f64).unwrap_or(0.5).clamp(0.1, 0.9);
    Ok(Node::Split {
        right,
        ratio,
        first: Box::new(parse_node(first, ws_dir, ws_env, home)?),
        second: Box::new(parse_node(second, ws_dir, ws_env, home)?),
    })
}

/// `~/x` in the home folder, absolute as is, anything else (`./x`, `x`, `.`
/// or nothing) under `base`.
fn resolve(path: Option<&str>, base: &Path, home: &Path) -> PathBuf {
    match path.map(str::trim).filter(|p| !p.is_empty() && *p != ".") {
        None => base.to_path_buf(),
        Some("~") => home.to_path_buf(),
        Some(p) if p.starts_with("~/") => home.join(&p[2..]),
        Some(p) if p.starts_with('/') => PathBuf::from(p),
        Some(p) => base.join(p.strip_prefix("./").unwrap_or(p)),
    }
}

fn env_of(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string)))
                .collect()
        })
        .unwrap_or_default()
}

/// Build the workspace in the background; the UI gets [`UiMsg::LayoutBuilt`]
/// with the tab to focus, or the error.
pub fn spawn_build(socket_path: PathBuf, spec: Spec, ui: async_channel::Sender<UiMsg>) {
    std::thread::spawn(move || {
        let result = Conn::connect(&socket_path).and_then(|mut conn| Builder { conn: &mut conn, setup: spec.setup.clone(), focus: None, first: None }.build(&spec));
        let message = match result {
            Ok(focus) => UiMsg::LayoutBuilt { name: spec.name.clone(), focus, error: None },
            Err(error) => UiMsg::LayoutBuilt { name: spec.name.clone(), focus: None, error: Some(error) },
        };
        let _ = ui.send_blocking(message);
    });
}

struct Builder<'a> {
    conn: &'a mut Conn,
    /// Still to run, in the first terminal.
    setup: Option<String>,
    /// The surface marked `focus`, else the first one made.
    focus: Option<u64>,
    first: Option<u64>,
}

impl Builder<'_> {
    fn build(mut self, spec: &Spec) -> Result<Option<u64>, String> {
        let created = self.conn.request(json!({ "cmd": "new-workspace", "name": spec.name }))?;
        let surface = created.get("surface").and_then(Value::as_u64).ok_or("new-workspace returned no tab")?;
        let pane = self.pane_of(surface)?;
        self.node(&spec.layout, pane, Some(surface))?;
        Ok(self.focus.or(self.first))
    }

    fn node(&mut self, node: &Node, pane: u64, initial: Option<u64>) -> Result<(), String> {
        match node {
            Node::Split { right, ratio, first, second } => {
                let made = self.conn.request(json!({ "cmd": "split", "pane": pane, "dir": if *right { "right" } else { "down" } }))?;
                let surface = made.get("surface").and_then(Value::as_u64).ok_or("split returned no tab")?;
                let new_pane = self.pane_of(surface)?;
                if let Some(split) = self.split_between(pane, new_pane)? {
                    self.conn.request(json!({ "cmd": "set-split-ratio", "split": split, "ratio": ratio }))?;
                }
                self.node(first, pane, initial)?;
                self.node(second, new_pane, Some(surface))
            }
            Node::Pane(surfaces) => {
                let mut leftover = initial;
                let mut focus_index = None;
                for (index, surface) in surfaces.iter().enumerate() {
                    let id = match (surface.browser, leftover) {
                        // The pane's first terminal is the one it came with.
                        (false, Some(existing)) if index == 0 => {
                            leftover = None;
                            self.start_terminal(existing, surface, true)?;
                            existing
                        }
                        (false, _) => {
                            let made = self.conn.request(json!({ "cmd": "new-tab", "pane": pane, "cwd": surface.cwd.display().to_string() }))?;
                            let id = made.get("surface").and_then(Value::as_u64).ok_or("new-tab returned no tab")?;
                            self.start_terminal(id, surface, false)?;
                            id
                        }
                        (true, _) => {
                            let url = surface.url.clone().unwrap_or_default();
                            let made = self.conn.request(json!({ "cmd": "new-browser-tab", "pane": pane, "url": url }))?;
                            let id = made.get("surface").and_then(Value::as_u64).ok_or("new-browser-tab returned no tab")?;
                            self.rename(id, surface)?;
                            id
                        }
                    };
                    self.first.get_or_insert(id);
                    if surface.focus && self.focus.is_none() {
                        self.focus = Some(id);
                        focus_index = Some(index);
                    }
                }
                // A pane that starts with a browser drops the terminal it
                // came with (and ends its shell).
                if let Some(unused) = leftover {
                    let terminal = self.terminal_of(unused)?;
                    self.conn.request(json!({ "cmd": "close-surface", "surface": unused }))?;
                    if let Some(terminal) = terminal {
                        let _ = self.conn.request(json!({ "cmd": "close-terminal", "terminal_id": terminal }));
                    }
                }
                // Show the focused tab, else the first.
                self.conn.request(json!({ "cmd": "select-tab", "pane": pane, "index": focus_index.unwrap_or(0) }))?;
                Ok(())
            }
        }
    }

    /// Name the tab, then type the start-up prefix, `setup` and the command
    /// into its shell. `cd` is needed for terminals that didn't get a cwd
    /// when they were made.
    fn start_terminal(&mut self, surface: u64, spec: &Surface, needs_cd: bool) -> Result<(), String> {
        self.rename(surface, spec)?;
        let mut prefix = Vec::new();
        if needs_cd {
            prefix.push(format!("cd -- {}", quote(&spec.cwd.display().to_string())));
        }
        for (key, value) in &spec.env {
            prefix.push(format!("export {key}={}", quote(value)));
        }
        let mut lines = Vec::new();
        if !prefix.is_empty() {
            prefix.push("clear".to_string());
            lines.push(format!(" {}", prefix.join(" && ")));
        }
        if let Some(setup) = self.setup.take() {
            lines.push(setup);
        }
        if let Some(command) = &spec.command {
            lines.push(command.clone());
        }
        for line in lines {
            self.conn.request(json!({ "cmd": "send", "surface": surface, "text": format!("{line}\r") }))?;
        }
        Ok(())
    }

    fn rename(&mut self, surface: u64, spec: &Surface) -> Result<(), String> {
        if let Some(name) = &spec.name {
            self.conn.request(json!({ "cmd": "rename-surface", "surface": surface, "name": name }))?;
        }
        Ok(())
    }

    fn tree(&mut self) -> Result<Value, String> {
        let data = self.conn.request(json!({ "cmd": "list-workspaces" }))?;
        Ok(data.get("workspaces").cloned().unwrap_or(Value::Null))
    }

    /// Every pane object in the session.
    fn panes(tree: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        for ws in tree.as_array().into_iter().flatten() {
            for screen in ws.get("screens").and_then(Value::as_array).into_iter().flatten() {
                out.extend(screen.get("panes").and_then(Value::as_array).into_iter().flatten().cloned());
            }
        }
        out
    }

    fn pane_of(&mut self, surface: u64) -> Result<u64, String> {
        let tree = self.tree()?;
        Self::panes(&tree)
            .iter()
            .find(|p| {
                p.get("tabs")
                    .and_then(Value::as_array)
                    .is_some_and(|tabs| tabs.iter().any(|t| t.get("surface").and_then(Value::as_u64) == Some(surface)))
            })
            .and_then(|p| p.get("id").and_then(Value::as_u64))
            .ok_or_else(|| format!("tab {surface} isn't in any pane"))
    }

    fn terminal_of(&mut self, surface: u64) -> Result<Option<String>, String> {
        let tree = self.tree()?;
        Ok(Self::panes(&tree).iter().find_map(|p| {
            p.get("tabs")?.as_array()?.iter().find_map(|t| {
                (t.get("surface").and_then(Value::as_u64) == Some(surface))
                    .then(|| t.get("terminal_id").and_then(Value::as_str).map(str::to_string))
                    .flatten()
            })
        }))
    }

    /// The split directly holding leaves `a` and `b` (just made by
    /// splitting `a`).
    fn split_between(&mut self, a: u64, b: u64) -> Result<Option<u64>, String> {
        fn find(node: &Value, a: u64, b: u64) -> Option<u64> {
            if node.get("type").and_then(Value::as_str) != Some("split") {
                return None;
            }
            let leaf = |n: Option<&Value>| n.and_then(|n| n.get("pane")).and_then(Value::as_u64);
            if leaf(node.get("a")) == Some(a) && leaf(node.get("b")) == Some(b) {
                return node.get("split").and_then(Value::as_u64);
            }
            node.get("a").and_then(|n| find(n, a, b)).or_else(|| node.get("b").and_then(|n| find(n, a, b)))
        }
        let tree = self.tree()?;
        for ws in tree.as_array().into_iter().flatten() {
            for screen in ws.get("screens").and_then(Value::as_array).into_iter().flatten() {
                let layouts = screen.get("layout").into_iter().chain(
                    screen.get("columns").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("layout")),
                );
                for layout in layouts {
                    if let Some(id) = find(layout, a, b) {
                        return Ok(Some(id));
                    }
                }
            }
        }
        Ok(None)
    }
}

/// Single-quote for a POSIX shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A plain protocol connection: one request at a time.
struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
}

impl Conn {
    fn connect(path: &Path) -> Result<Self, String> {
        let writer = UnixStream::connect(path).map_err(|e| e.to_string())?;
        writer.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
        let reader = BufReader::new(writer.try_clone().map_err(|e| e.to_string())?);
        Ok(Self { reader, writer, next: 0 })
    }

    /// Send a command; its `data`, or the error.
    fn request(&mut self, mut request: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        request["id"] = json!(id);
        let command = request.get("cmd").and_then(Value::as_str).unwrap_or("request").to_string();
        let mut line = request.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        loop {
            let mut reply = String::new();
            if self.reader.read_line(&mut reply).map_err(|e| e.to_string())? == 0 {
                return Err("the session closed the connection".into());
            }
            let Ok(reply) = serde_json::from_str::<Value>(&reply) else { continue };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if reply.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(reply.get("data").cloned().unwrap_or(Value::Null));
            }
            let error = reply.get("error").and_then(Value::as_str).unwrap_or("failed");
            return Err(format!("{command}: {error}"));
        }
    }
}
