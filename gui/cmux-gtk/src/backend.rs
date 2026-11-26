//! Blocking cmux protocol I/O, kept off the GTK main thread.
//!
//! Two kinds of worker talk to the session socket:
//! - one control thread (upstream's Rust SDK) polls the workspace tree and
//!   agent roster and runs layout commands (new workspace/tab, split,
//!   select, close, acknowledge);
//! - one event thread subscribes to session events: notifications become
//!   [`UiMsg::Notification`], and tree/agent/notification events make the
//!   control thread re-poll at once;
//! - one [`Attachment`] per viewed terminal owns a byte-mode attach
//!   connection. Its reader forwards the VT replay and live output to the UI;
//!   its writer sends input and size reports **on that same connection**.
//!
//! The second point matters: the server sizes a terminal from its attached
//! viewers only, and each connection is a separate viewer. A resize sent on
//! any other connection comes back `accepted: false`. The SDK's raw stream
//! owns its connection without a way to send on it, so attachments speak the
//! protocol's newline-delimited JSON directly.
//!
//! Everything reaches the UI as [`UiMsg`] over an async channel.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use cmux::raw::{Client, ClientConfig, Error, NewWorkspaceRequest, Optional};
use serde_json::{Value, json};

/// How often the control thread re-reads the workspace tree.
const TREE_POLL: Duration = Duration::from_millis(1500);

/// A workspace: sidebar row plus the active screen's layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub id: u64,
    pub name: String,
    pub active: bool,
    /// Title of the active pane's active tab, for the sidebar subtitle.
    pub title: String,
    pub layout: Option<Layout>,
    pub panes: Vec<Pane>,
    pub active_pane: Option<u64>,
}

/// The active screen's split tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    Leaf(u64),
    /// `right`: `a` left of `b`; otherwise `a` above `b`. `ratio` is `a`'s share.
    Split { id: Option<u64>, right: bool, ratio: f64, a: Box<Layout>, b: Box<Layout> },
}

impl Layout {
    /// Structure only (pane ids, split ids, directions). Ratios are left out
    /// so dragging a divider never forces a rebuild.
    pub fn signature(&self) -> String {
        match self {
            Layout::Leaf(pane) => format!("p{pane}"),
            Layout::Split { id, right, a, b, .. } => {
                format!("s{}{}({},{})", id.unwrap_or(0), if *right { "r" } else { "d" }, a.signature(), b.signature())
            }
        }
    }

    pub fn panes(&self) -> Vec<u64> {
        match self {
            Layout::Leaf(pane) => vec![*pane],
            Layout::Split { a, b, .. } => [a.panes(), b.panes()].concat(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pane {
    pub id: u64,
    pub active_tab: usize,
    pub tabs: Vec<Tab>,
}

impl Pane {
    /// The terminal surface to show; `None` for a browser tab or no tabs.
    pub fn surface(&self) -> Option<u64> {
        self.tabs.get(self.active_tab).filter(|tab| tab.terminal).map(|tab| tab.surface)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub surface: u64,
    pub title: String,
    pub terminal: bool,
    /// Level of an unread notification on this tab (`info`, `warning`,
    /// `error`); cleared when the tab is selected or acknowledged.
    pub unread: Option<String>,
}

/// Agent status per terminal surface, in `list-agents`' one-vocabulary
/// `status` form: `blocked`, `done` (finished a turn nobody has seen yet),
/// `working`, `idle`, `unknown`.
pub type Agents = std::collections::HashMap<u64, String>;

/// Messages from workers to the UI.
pub enum UiMsg {
    /// The session answered again (first contact, or after a restart).
    Connected,
    /// The session stopped answering; the control thread keeps retrying.
    Disconnected(String),
    Tree(Vec<Workspace>, Agents),
    /// A posted notification (from `notify`, an agent hook, or the mux).
    Notification { id: u64, title: String, body: String, level: String, surface: Option<u64> },
    /// Output for the attachment with this `generation` (unique across all
    /// panes); events from replaced attachments are dropped by the UI.
    Attach { generation: u64, event: AttachEvent },
    Status(String),
}

pub enum AttachEvent {
    /// Full screen state: reset the widget, then feed `data`.
    Replay { data: Vec<u8>, colors: Option<Value> },
    Output(Vec<u8>),
    Colors(Value),
    Ended(String),
}

/// Requests from the UI to the control thread.
pub enum Ctl {
    NewWorkspace,
    /// Re-poll now (sent by the event thread when something changed).
    Poke,
    /// Any protocol command as a JSON object with `cmd` (e.g. `split`,
    /// `new-tab`, `select-tab`, `close-surface`, `focus-pane`).
    Request(Value),
}

impl Ctl {
    pub fn request(request: Value) -> Self {
        Ctl::Request(request)
    }
}

pub fn config_for(session: &str) -> Result<ClientConfig, Error> {
    Ok(ClientConfig::try_from_env_or_default_session(session)?.with_timeout(Duration::from_secs(10)))
}

/// Control thread: tree polling and layout requests, reconnecting forever.
///
/// The session is "up" once a workspace tree has been read; until then, and
/// whenever a poll fails (the server stopped or restarted), the UI gets
/// [`UiMsg::Disconnected`] once and the thread retries every second.
/// Requests sent while disconnected are dropped (the UI greys itself out).
pub fn spawn_control(config: ClientConfig, rx: Receiver<Ctl>, ui: async_channel::Sender<UiMsg>) {
    std::thread::spawn(move || {
        let mut announced_down = false;
        loop {
            let (lost, was_up) = match Client::connect(config.clone()) {
                Ok(mut client) => match serve(&mut client, &rx, &ui) {
                    Served::UiGone => return,
                    Served::Lost { reason, was_up } => (reason, was_up),
                },
                Err(error) => (error.to_string(), false),
            };
            // A new outage after a successful connection is announced again.
            if was_up {
                announced_down = false;
            }
            if !announced_down {
                announced_down = true;
                if ui.send_blocking(UiMsg::Disconnected(lost)).is_err() {
                    return;
                }
            }
            if !wait_and_drop(&rx, RETRY) {
                return;
            }
        }
    });
}

/// How long to wait between reconnect attempts.
const RETRY: Duration = Duration::from_secs(1);

enum Served {
    UiGone,
    /// The connection failed; `was_up` if a tree had been read on it.
    Lost { reason: String, was_up: bool },
}

/// Poll and run requests on one connection until it fails.
fn serve(client: &mut Client, rx: &Receiver<Ctl>, ui: &async_channel::Sender<UiMsg>) -> Served {
    let mut last: Option<(Vec<Workspace>, Agents)> = None;
    loop {
        let snapshot = client.workspace_tree().and_then(|tree| {
            let workspaces =
                serde_json::to_value(&tree).map(|value| parse_workspaces(&value)).unwrap_or_default();
            Ok((workspaces, list_agents(client)?))
        });
        match snapshot {
            Ok(snapshot) => {
                if last.is_none() && ui.send_blocking(UiMsg::Connected).is_err() {
                    return Served::UiGone;
                }
                if last.as_ref() != Some(&snapshot) {
                    last = Some(snapshot.clone());
                    if ui.send_blocking(UiMsg::Tree(snapshot.0, snapshot.1)).is_err() {
                        return Served::UiGone;
                    }
                }
            }
            Err(error) => return Served::Lost { reason: error.to_string(), was_up: last.is_some() },
        }
        // Wait for a request or the next poll. Run everything queued (a burst
        // of events coalesces into one re-poll), then loop straight back to
        // re-read the tree so the UI updates at once.
        let first = match rx.recv_timeout(TREE_POLL) {
            Ok(ctl) => ctl,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Served::UiGone,
        };
        for ctl in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            let result = match ctl {
                Ctl::Poke => Ok(()),
                Ctl::NewWorkspace => client
                    .new_workspace(NewWorkspaceRequest {
                        name: Optional::Value("gtk".to_string()),
                        ..Default::default()
                    })
                    .map(|_| ()),
                Ctl::Request(Value::Object(request)) => client.request_raw(request).map(|_| ()),
                Ctl::Request(_) => Ok(()),
            };
            if let Err(error) = result {
                let _ = ui.send_blocking(UiMsg::Status(format!("request failed: {error}")));
            }
        }
    }
}

/// The agent roster as surface → status. `status` is the vocabulary every
/// client shows; older daemons only send `state`.
fn list_agents(client: &mut Client) -> Result<Agents, Error> {
    let mut request = serde_json::Map::new();
    request.insert("cmd".into(), json!("list-agents"));
    let response = client.request_raw(request)?;
    let list = response.get("agents").or_else(|| response.pointer("/data/agents"));
    Ok(list
        .and_then(Value::as_array)
        .map(|agents| {
            agents
                .iter()
                .filter_map(|agent| {
                    let surface = agent.get("surface")?.as_u64()?;
                    let status = agent.get("status").and_then(Value::as_str).or_else(|| agent.get("state")?.as_str())?;
                    Some((surface, status.to_string()))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Event thread: subscribe to session events, reconnecting forever.
pub fn spawn_events(socket_path: PathBuf, ctl: mpsc::Sender<Ctl>, ui: async_channel::Sender<UiMsg>) {
    std::thread::spawn(move || {
        loop {
            if let Ok(socket) = UnixStream::connect(&socket_path) {
                let mut writer = socket.try_clone().ok();
                let subscribed = writer
                    .as_mut()
                    .is_some_and(|w| w.write_all(b"{\"id\":1,\"cmd\":\"subscribe\"}\n").is_ok());
                if subscribed {
                    for line in BufReader::new(socket).lines() {
                        let Ok(line) = line else { break };
                        let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                        let name = event.get("event").and_then(Value::as_str).unwrap_or("");
                        if name == "notification" {
                            let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                            let message = UiMsg::Notification {
                                id: event.get("notification").and_then(Value::as_u64).unwrap_or(0),
                                title: text("title"),
                                body: text("body"),
                                level: text("level"),
                                surface: event.get("surface").and_then(Value::as_u64),
                            };
                            if ui.send_blocking(message).is_err() {
                                return;
                            }
                        }
                        if matches!(name, "notification" | "tree-changed" | "agent-changed") && ctl.send(Ctl::Poke).is_err() {
                            return;
                        }
                    }
                }
            }
            if ui.is_closed() {
                return;
            }
            std::thread::sleep(RETRY);
        }
    });
}

/// Sleep for `wait`, discarding requests that arrive meanwhile. Returns false
/// when the UI has gone away.
fn wait_and_drop(rx: &Receiver<Ctl>, wait: Duration) -> bool {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        match rx.recv_timeout(left) {
            Ok(_) => {} // no session to send it to
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// A live byte-mode attachment to one terminal surface.
///
/// Dropping it closes the connection, which also removes this viewer's size
/// report on the server.
pub struct Attachment {
    surface: u64,
    writer: mpsc::Sender<Value>,
    socket: UnixStream,
}

impl Attachment {
    /// Connect and attach. Events go to `ui` tagged with `generation`.
    pub fn open(
        socket_path: &PathBuf,
        surface: u64,
        (cols, rows): (u16, u16),
        generation: u64,
        ui: async_channel::Sender<UiMsg>,
    ) -> std::io::Result<Self> {
        let socket = UnixStream::connect(socket_path)?;
        let read_half = socket.try_clone()?;
        let mut write_half = socket.try_clone()?;

        let (writer, requests) = mpsc::channel::<Value>();
        // Writer: assigns request ids and writes one JSON object per line.
        std::thread::spawn(move || {
            for (id, mut request) in (1u64..).zip(requests) {
                request["id"] = json!(id);
                let line = format!("{request}\n");
                if write_half.write_all(line.as_bytes()).is_err() {
                    return;
                }
            }
        });
        // Reader: events become UI messages; responses are only checked for
        // errors (the attach request's own failure arrives this way).
        std::thread::spawn(move || {
            let send = |event| ui.send_blocking(UiMsg::Attach { generation, event }).is_ok();
            for line in BufReader::new(read_half).lines() {
                let Ok(line) = line else { return }; // closed (switched away)
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                if message.get("event").is_none() {
                    if message.get("ok") == Some(&Value::Bool(false)) {
                        let error = message.get("error").and_then(Value::as_str).unwrap_or("request failed");
                        if !send(AttachEvent::Ended(format!("server: {error}"))) {
                            return;
                        }
                    }
                    continue;
                }
                if !forward(&message, &send) {
                    return;
                }
            }
        });

        let attachment = Self { surface, writer, socket };
        attachment.request(json!({
            "cmd": "attach-surface", "surface": surface, "mode": "bytes", "cols": cols, "rows": rows,
        }));
        Ok(attachment)
    }

    /// Report this viewer's cell grid; the server reflows the terminal when
    /// this viewer owns the size (the most recently active viewer does).
    pub fn resize(&self, cols: u16, rows: u16) {
        self.request(json!({ "cmd": "resize-surface", "surface": self.surface, "cols": cols, "rows": rows }));
    }

    /// Send keyboard input or a paste. Input also counts as viewer activity,
    /// which hands size ownership to this window.
    pub fn send(&self, bytes: &[u8]) {
        self.request(json!({ "cmd": "send", "surface": self.surface, "bytes": B64.encode(bytes) }));
    }

    fn request(&self, request: Value) {
        let _ = self.writer.send(request);
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

/// Translate one attach event. Returns false when the attachment ended or
/// the UI is gone.
fn forward(message: &Value, send: &impl Fn(AttachEvent) -> bool) -> bool {
    let data = |key: &str| message.get(key).and_then(Value::as_str).map(decode);
    let colors = || message.get("colors").filter(|c| c.is_object()).cloned();
    match message.get("event").and_then(Value::as_str).unwrap_or("") {
        "vt-state" => send(AttachEvent::Replay { data: data("data").unwrap_or_default(), colors: colors() }),
        // `replay` is current; `data` is the protocol 6 compatibility field.
        "resized" => match data("replay").or_else(|| data("data")) {
            Some(data) => send(AttachEvent::Replay { data, colors: colors() }),
            None => true,
        },
        "output" => send(AttachEvent::Output(data("data").unwrap_or_default())),
        "colors-changed" => send(AttachEvent::Colors(message.clone())),
        "detached" => {
            send(AttachEvent::Ended("terminal detached".into()));
            false
        }
        "overflow" => {
            let error = message.get("error").and_then(Value::as_str).unwrap_or("overflow");
            send(AttachEvent::Ended(format!("output overflow ({error}); reselect to resume")));
            false
        }
        _ => true,
    }
}

fn decode(data: &str) -> Vec<u8> {
    B64.decode(data).unwrap_or_default()
}

/// Flatten the protocol tree. Works on JSON so the prototype depends only on
/// the fields it reads. Only each workspace's active screen is modelled.
fn parse_workspaces(tree: &Value) -> Vec<Workspace> {
    let Some(list) = tree.get("workspaces").and_then(Value::as_array) else { return Vec::new() };
    list.iter()
        .map(|ws| {
            let screen = active(ws.get("screens"), |s| s.get("active").and_then(Value::as_bool) == Some(true));
            let panes: Vec<Pane> = screen
                .and_then(|s| s.get("panes"))
                .and_then(Value::as_array)
                .map(|panes| panes.iter().filter_map(parse_pane).collect())
                .unwrap_or_default();
            let active_pane = screen.and_then(|s| s.get("active_pane")).and_then(Value::as_u64);
            let title = active_pane
                .and_then(|id| panes.iter().find(|p| p.id == id))
                .and_then(|p| p.tabs.get(p.active_tab))
                .map(|t| t.title.clone())
                .unwrap_or_default();
            Workspace {
                id: ws.get("id").and_then(Value::as_u64).unwrap_or(0),
                name: ws.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                active: ws.get("active").and_then(Value::as_bool).unwrap_or(false),
                title,
                layout: screen.and_then(|s| s.get("layout")).and_then(parse_layout),
                panes,
                active_pane,
            }
        })
        .collect()
}

fn parse_pane(pane: &Value) -> Option<Pane> {
    let tabs = pane
        .get("tabs")
        .and_then(Value::as_array)
        .map(|tabs| {
            tabs.iter()
                .filter_map(|tab| {
                    let marker = tab.get("notification").filter(|m| m.is_object());
                    Some(Tab {
                        surface: tab.get("surface")?.as_u64()?,
                        title: tab.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                        terminal: tab.get("kind").and_then(Value::as_str) == Some("pty"),
                        unread: marker
                            .filter(|m| m.get("unread").and_then(Value::as_bool) != Some(false))
                            .map(|m| m.get("level").and_then(Value::as_str).unwrap_or("info").to_string()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Pane {
        id: pane.get("id")?.as_u64()?,
        active_tab: pane.get("active_tab").and_then(Value::as_u64).unwrap_or(0) as usize,
        tabs,
    })
}

fn parse_layout(node: &Value) -> Option<Layout> {
    match node.get("type")?.as_str()? {
        "leaf" => Some(Layout::Leaf(node.get("pane")?.as_u64()?)),
        "split" => Some(Layout::Split {
            id: node.get("split").and_then(Value::as_u64),
            right: node.get("dir").and_then(Value::as_str) == Some("right"),
            ratio: node.get("ratio").and_then(Value::as_f64).unwrap_or(0.5),
            a: Box::new(parse_layout(node.get("a")?)?),
            b: Box::new(parse_layout(node.get("b")?)?),
        }),
        // A stack shows one expanded pane at a time.
        "stack" => Some(Layout::Leaf(node.get("expanded")?.as_u64()?)),
        _ => None,
    }
}

/// The element matching `pred`, else the first one.
fn active(list: Option<&Value>, pred: impl Fn(&Value) -> bool) -> Option<&Value> {
    let list = list?.as_array()?;
    list.iter().find(|v| pred(v)).or_else(|| list.first())
}
