//! Blocking cmux protocol I/O, kept off the GTK main thread.
//!
//! Two kinds of worker talk to the session socket:
//! - one control thread (upstream's Rust SDK) polls the workspace tree and
//!   creates workspaces;
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

/// A workspace as the sidebar shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub id: u64,
    pub name: String,
    pub active: bool,
    /// The terminal surface to show: the active tab of the active pane of
    /// the active screen. `None` when that tab is a browser or missing.
    pub surface: Option<u64>,
    pub title: String,
}

/// Messages from workers to the UI.
pub enum UiMsg {
    Tree(Vec<Workspace>),
    /// Output for the attachment started with `generation`; stale
    /// generations are dropped by the UI after a switch.
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
}

pub fn config_for(session: &str) -> Result<ClientConfig, Error> {
    Ok(ClientConfig::try_from_env_or_default_session(session)?.with_timeout(Duration::from_secs(10)))
}

/// Control thread: tree polling plus workspace creation.
pub fn spawn_control(config: ClientConfig, rx: Receiver<Ctl>, ui: async_channel::Sender<UiMsg>) {
    std::thread::spawn(move || {
        let mut client = match Client::connect(config) {
            Ok(client) => client,
            Err(error) => {
                let _ = ui.send_blocking(UiMsg::Status(format!("cannot connect: {error}")));
                return;
            }
        };
        let mut last: Option<Vec<Workspace>> = None;
        loop {
            match client.workspace_tree() {
                Ok(tree) => {
                    let workspaces = serde_json::to_value(&tree)
                        .map(|value| parse_workspaces(&value))
                        .unwrap_or_default();
                    if last.as_ref() != Some(&workspaces) {
                        last = Some(workspaces.clone());
                        if ui.send_blocking(UiMsg::Tree(workspaces)).is_err() {
                            return; // UI gone
                        }
                    }
                }
                Err(error) => {
                    let _ = ui.send_blocking(UiMsg::Status(format!("session lost: {error}")));
                    return;
                }
            }
            match rx.recv_timeout(TREE_POLL) {
                Ok(Ctl::NewWorkspace) => {
                    let result = client.new_workspace(NewWorkspaceRequest {
                        name: Optional::Value("gtk".to_string()),
                        ..Default::default()
                    });
                    if let Err(error) = result {
                        let _ = ui.send_blocking(UiMsg::Status(format!("new workspace failed: {error}")));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
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

/// Flatten the protocol tree into sidebar rows. Works on JSON so the
/// prototype only depends on the handful of fields it reads.
fn parse_workspaces(tree: &Value) -> Vec<Workspace> {
    let Some(list) = tree.get("workspaces").and_then(Value::as_array) else { return Vec::new() };
    list.iter()
        .map(|ws| {
            let screen = active(ws.get("screens"), |s| s.get("active").and_then(Value::as_bool) == Some(true));
            let pane = screen.and_then(|screen| {
                let want = screen.get("active_pane").and_then(Value::as_u64);
                active(screen.get("panes"), |p| p.get("id").and_then(Value::as_u64) == want)
            });
            let tab = pane.and_then(|pane| {
                let index = pane.get("active_tab").and_then(Value::as_u64).unwrap_or(0) as usize;
                pane.get("tabs").and_then(Value::as_array).and_then(|tabs| tabs.get(index))
            });
            let surface = tab
                .filter(|tab| tab.get("kind").and_then(Value::as_str) == Some("pty"))
                .and_then(|tab| tab.get("surface").and_then(Value::as_u64));
            Workspace {
                id: ws.get("id").and_then(Value::as_u64).unwrap_or(0),
                name: ws.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                active: ws.get("active").and_then(Value::as_bool).unwrap_or(false),
                surface,
                title: tab.and_then(|t| t.get("title")).and_then(Value::as_str).unwrap_or("").to_string(),
            }
        })
        .collect()
}

/// The element matching `pred`, else the first one.
fn active(list: Option<&Value>, pred: impl Fn(&Value) -> bool) -> Option<&Value> {
    let list = list?.as_array()?;
    list.iter().find(|v| pred(v)).or_else(|| list.first())
}
