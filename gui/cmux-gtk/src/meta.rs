//! Workspace details for the sidebar: the git branch of the workspace's
//! active terminal and the TCP ports its processes listen on (a dev server,
//! say), like the macOS app's sidebar.
//!
//! The server doesn't track either, but the window runs on the same machine
//! as the session, so a background thread works them out every few seconds:
//! - `process-info` gives each terminal's shell pid and working directory;
//! - the branch comes from the repository around that directory (`HEAD`,
//!   read directly; worktrees and detached heads included), with no `git`
//!   process;
//! - ports are the listening TCP sockets (`/proc/net/tcp`, `tcp6`) owned by
//!   the shell or anything started under it (`/proc/<pid>/fd`). A process
//!   that detached itself from the shell (daemonized) isn't found.
//!
//! Results reach the UI as [`UiMsg::Meta`], only when they change.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::backend::UiMsg;

/// What one workspace shows under its name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WsMeta {
    /// The active terminal's working directory.
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Listening TCP ports of the workspace's terminals, sorted.
    pub ports: Vec<u16>,
}

/// One workspace to look at: its active terminal (for the directory and
/// branch) and every terminal in it (for ports).
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub workspace: u64,
    pub primary: Option<u64>,
    pub surfaces: Vec<u64>,
}

/// The current targets, set by the UI whenever the tree changes.
pub type Targets = Arc<Mutex<Vec<Target>>>;

const INTERVAL: Duration = Duration::from_secs(3);

pub fn spawn(socket_path: PathBuf, targets: Targets, ui: async_channel::Sender<UiMsg>) {
    std::thread::spawn(move || {
        let mut conn: Option<Conn> = None;
        let mut last: Option<HashMap<u64, WsMeta>> = None;
        loop {
            if ui.is_closed() {
                return;
            }
            let wanted = targets.lock().map(|t| t.clone()).unwrap_or_default();
            if conn.is_none() {
                conn = Conn::connect(&socket_path).ok();
            }
            if let Some(c) = conn.as_mut() {
                match collect(c, &wanted) {
                    Ok(meta) => {
                        if last.as_ref() != Some(&meta) {
                            last = Some(meta.clone());
                            if ui.send_blocking(UiMsg::Meta(meta)).is_err() {
                                return;
                            }
                        }
                    }
                    // The session went away; reconnect next round.
                    Err(_) => conn = None,
                }
            }
            std::thread::sleep(INTERVAL);
        }
    });
}

/// One round: details for every target workspace.
fn collect(conn: &mut Conn, targets: &[Target]) -> Result<HashMap<u64, WsMeta>, String> {
    let listening = listening_sockets();
    let children = child_map();
    let mut out = HashMap::new();
    for target in targets {
        let mut meta = WsMeta::default();
        let mut pids = Vec::new();
        for surface in &target.surfaces {
            let Some((pid, cwd)) = conn.process_info(*surface)? else { continue };
            pids.push(pid);
            if Some(*surface) == target.primary {
                meta.branch = cwd.as_deref().and_then(|d| git_branch(Path::new(d)));
                meta.cwd = cwd;
            }
        }
        if !listening.is_empty() {
            let mut ports: Vec<u16> = descendants(&pids, &children)
                .into_iter()
                .flat_map(socket_inodes)
                .filter_map(|inode| listening.get(&inode).copied())
                .collect();
            ports.sort_unstable();
            ports.dedup();
            meta.ports = ports;
        }
        out.insert(target.workspace, meta);
    }
    Ok(out)
}

/// A plain protocol connection for `process-info`.
struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
}

impl Conn {
    fn connect(path: &Path) -> Result<Self, String> {
        let writer = UnixStream::connect(path).map_err(|e| e.to_string())?;
        writer.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        let reader = BufReader::new(writer.try_clone().map_err(|e| e.to_string())?);
        Ok(Self { reader, writer, next: 0 })
    }

    /// The terminal's shell pid and working directory (the foreground
    /// program's if it reports one). `None` for a terminal that has gone.
    fn process_info(&mut self, surface: u64) -> Result<Option<(u32, Option<String>)>, String> {
        self.next += 1;
        let id = self.next;
        let mut line = json!({ "id": id, "cmd": "process-info", "surface": surface }).to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        loop {
            let mut reply = String::new();
            if self.reader.read_line(&mut reply).map_err(|e| e.to_string())? == 0 {
                return Err("connection closed".into());
            }
            let Ok(reply) = serde_json::from_str::<Value>(&reply) else { continue };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            let data = reply.get("data").cloned().unwrap_or(Value::Null);
            let Some(pid) = data.get("pid").and_then(Value::as_u64) else { return Ok(None) };
            let cwd = ["foreground_cwd", "cwd"]
                .iter()
                .find_map(|key| data.get(*key).and_then(Value::as_str).filter(|c| !c.is_empty()))
                .map(str::to_string);
            return Ok(Some((pid as u32, cwd)));
        }
    }
}

/// The branch checked out in the repository around `dir`, or the short
/// commit id when the head is detached.
fn git_branch(dir: &Path) -> Option<String> {
    let mut at = Some(dir);
    while let Some(here) = at {
        let dot_git = here.join(".git");
        let git_dir = if dot_git.is_dir() {
            Some(dot_git)
        } else if dot_git.is_file() {
            // A worktree or submodule: `gitdir: <path>`.
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
            Some(if target.is_absolute() { target } else { here.join(target) })
        } else {
            None
        };
        if let Some(git_dir) = git_dir {
            let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
            let head = head.trim();
            return Some(match head.strip_prefix("ref:") {
                Some(reference) => {
                    let reference = reference.trim();
                    reference.strip_prefix("refs/heads/").unwrap_or(reference).to_string()
                }
                None => head.chars().take(7).collect(),
            });
        }
        at = here.parent();
    }
    None
}

/// Listening TCP sockets on this machine: inode → port.
fn listening_sockets() -> HashMap<u64, u16> {
    let mut out = HashMap::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // local_address, state 0A = LISTEN, inode.
            if fields.len() < 10 || fields[3] != "0A" {
                continue;
            }
            let port = fields[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok());
            if let (Some(port), Ok(inode)) = (port, fields[9].parse::<u64>()) {
                out.insert(inode, port);
            }
        }
    }
    out
}

/// Every process's children: ppid → pids.
fn child_map() -> HashMap<u32, Vec<u32>> {
    let mut out: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return out };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
        // `pid (comm) state ppid …`; comm may hold spaces and parentheses.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { continue };
        if let Some(ppid) = rest.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok()) {
            out.entry(ppid).or_default().push(pid);
        }
    }
    out
}

/// The given processes and everything under them.
fn descendants(roots: &[u32], children: &HashMap<u32, Vec<u32>>) -> HashSet<u32> {
    let mut seen = HashSet::new();
    let mut stack = roots.to_vec();
    while let Some(pid) = stack.pop() {
        if seen.insert(pid) {
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids);
            }
        }
    }
    seen
}

/// The socket inodes a process holds open.
fn socket_inodes(pid: u32) -> Vec<u64> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    fds.flatten()
        .filter_map(|fd| {
            let target = std::fs::read_link(fd.path()).ok()?;
            let target = target.to_str()?;
            target.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
        })
        .collect()
}
