//! A Linux browser provider for cmux browser tabs.
//!
//! cmux-tui never runs a browser itself. A browser tab is a canonical tab
//! whose page lives in a *provider*: a process that owns a browser, opens one
//! page target per browser tab, and leases them to the session with
//! `register-browser-provider` (endpoint + `tab_id → target_id` map). The
//! session then attaches over CDP and streams frames to every client — the
//! TUI, this GTK app, or agents. Upstream's provider is cmux-browser, a
//! patched Chromium that isn't publicly buildable yet; this one runs a stock
//! Chromium-family browser headless instead.
//!
//! It needs a Chromium-family browser, because the session talks to pages
//! over the Chrome DevTools Protocol (Firefox no longer speaks it). Open-
//! source builds are preferred over Google Chrome. Without one, terminals are
//! unaffected; browser tabs report why they can't start.
//!
//! The lease is tied to the registering connection, so the provider keeps it
//! open, re-reads the tree every second, opens or closes pages as browser
//! tabs come and go, re-registers the full set when it changes, and
//! reconnects after a session restart (pages survive in the browser). It is
//! started lazily, the first time a browser tab exists and no other provider
//! is registered.
//!
//! The browser must never outlive us, however we exit (including SIGKILL or
//! a crash). A parent-death signal isn't enough: launcher scripts such as
//! Brave's run the real browser as a grandchild, which doesn't inherit it.
//! So the browser also gets `--remote-debugging-pipe` on fds 3/4: Chromium
//! shuts down when that pipe closes, the kernel closes our end whenever this
//! process dies, and the pipe is inherited straight through launchers.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PROVIDER_ID: &str = "cmux-lin-chrome";
const POLL: Duration = Duration::from_secs(1);

/// Running provider; dropping it stops the thread, unregisters, and closes
/// the browser.
pub struct ProviderHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ProviderHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the provider tells its host.
pub enum Report {
    /// A one-line status message.
    Info(String),
    /// Browser tabs can't start, and why (with the fix).
    Problem(String),
    /// A previous problem is resolved.
    Resolved,
}

/// How long to wait before trying again to start a browser that failed.
const LAUNCH_RETRY: Duration = Duration::from_secs(30);

/// Start providing browser tabs for the session at `socket_path`.
pub fn start(socket_path: PathBuf, report: impl Fn(Report) + Send + 'static) -> ProviderHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("browser-provider".into())
        .spawn(move || run(&socket_path, &thread_stop, &report))
        .expect("spawn provider thread");
    ProviderHandle { stop, thread: Some(thread) }
}

struct Chrome {
    child: Child,
    /// Our ends of the debugging pipe; closing them shuts the browser down.
    _pipe: [std::os::fd::OwnedFd; 2],
    /// Binary file name, for messages.
    name: String,
    port: u16,
    endpoint: String,
}

impl Drop for Chrome {
    fn drop(&mut self) {
        // Fields drop after this body, so close the pipe first by hand: the
        // browser sees EOF and exits (also when the launcher forked it),
        // then reap the direct child.
        let pipe = std::mem::replace(&mut self._pipe, closed_pair());
        drop(pipe);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Two descriptors on /dev/null, to swap in when closing the real pipe.
fn closed_pair() -> [std::os::fd::OwnedFd; 2] {
    let null = || std::fs::File::open("/dev/null").map(std::os::fd::OwnedFd::from).expect("open /dev/null");
    [null(), null()]
}

/// A close-on-exec pipe as (read end, write end).
fn pipe() -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd), String> {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1])) })
}

fn run(socket_path: &Path, stop: &AtomicBool, report: &dyn Fn(Report)) {
    let log = |line: String| report(Report::Info(line));
    let mut chrome: Option<Chrome> = None;
    // The launch problem last reported, so it's reported once, not on every
    // retry; cleared (and announced) when a browser starts.
    let mut problem: Option<String> = None;
    // tab_id → page target id
    let mut pages: HashMap<String, String> = HashMap::new();
    let mut announced_foreign = false;
    while !stop.load(Ordering::Acquire) {
        let Ok(mut session) = Session::connect(socket_path) else {
            sleep(stop, POLL);
            continue;
        };
        let mut registered: Option<Vec<(String, String)>> = None;
        loop {
            if stop.load(Ordering::Acquire) {
                let _ = session.request(json!({ "cmd": "unregister-browser-provider" }));
                return;
            }
            let Ok(tabs) = session.browser_tabs() else { break }; // session gone
            // Another provider (cmux-browser, a standalone
            // cmux-browser-provider, or a second cmux-gtk) owns the lease:
            // stay out of its way. Leases belong to the registering
            // connection, so any provider seen before we register is theirs.
            if registered.is_none() {
                match session.request(json!({ "cmd": "get-browser-provider" })) {
                    Ok(state) if state.get("available") == Some(&Value::Bool(true)) => {
                        if !announced_foreign {
                            announced_foreign = true;
                            log("browser tabs are provided by another browser provider".into());
                        }
                        sleep(stop, POLL);
                        continue;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            if tabs.is_empty() && chrome.is_none() {
                sleep(stop, POLL);
                continue;
            }
            if chrome.as_mut().is_none_or(|c| !matches!(c.child.try_wait(), Ok(None))) {
                pages.clear(); // a new Chrome has none of the old pages
                match launch_chrome() {
                    Ok(started) => {
                        log(format!("browser provider: {} on 127.0.0.1:{}", started.name, started.port));
                        if problem.take().is_some() {
                            report(Report::Resolved);
                        }
                        chrome = Some(started);
                    }
                    Err(error) => {
                        if problem.as_deref() != Some(error.as_str()) {
                            report(Report::Problem(error.clone()));
                            problem = Some(error);
                        }
                        // Retry quietly: the browser may get installed, or
                        // CMUX_LIN_BROWSER set for a restarted provider.
                        sleep(stop, LAUNCH_RETRY);
                        continue;
                    }
                }
            }
            let browser = chrome.as_ref().expect("chrome running");

            // Open pages for new tabs, close pages of tabs that are gone.
            for (tab_id, url) in &tabs {
                if !pages.contains_key(tab_id) {
                    match open_page(browser.port, url) {
                        Ok(target) => {
                            pages.insert(tab_id.clone(), target);
                        }
                        Err(error) => log(format!("browser provider: cannot open {url}: {error}")),
                    }
                }
            }
            let live: Vec<String> = tabs.iter().map(|(id, _)| id.clone()).collect();
            pages.retain(|tab_id, target| {
                let keep = live.contains(tab_id);
                if !keep {
                    let _ = http(browser.port, "GET", &format!("/json/close/{target}"));
                }
                keep
            });

            let mut set: Vec<(String, String)> = pages.iter().map(|(t, p)| (t.clone(), p.clone())).collect();
            set.sort();
            if registered.as_ref() != Some(&set) {
                let targets: Vec<Value> =
                    set.iter().map(|(tab, target)| json!({ "tab_id": tab, "target_id": target })).collect();
                let result = session.request(json!({
                    "cmd": "register-browser-provider",
                    "provider_id": PROVIDER_ID,
                    "endpoint": browser.endpoint,
                    "authentication": "none",
                    "targets": targets,
                }));
                match result {
                    Ok(_) => registered = Some(set),
                    Err(error) => {
                        log(format!("browser provider: registration failed: {error}"));
                        break;
                    }
                }
            }
            sleep(stop, POLL);
        }
        sleep(stop, POLL);
    }
}

fn sleep(stop: &AtomicBool, total: Duration) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::Acquire) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One control connection, newline-delimited JSON, no subscriptions (so
/// every line is a response).
struct Session {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Session {
    fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: stream, next_id: 1 })
    }

    fn request(&mut self, mut request: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        request["id"] = json!(id);
        self.writer.write_all(format!("{request}\n").as_bytes()).map_err(|e| e.to_string())?;
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err("session closed".into());
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if message.get("ok") == Some(&Value::Bool(true)) {
                return Ok(message.get("data").cloned().unwrap_or(Value::Null));
            }
            return Err(message.get("error").and_then(Value::as_str).unwrap_or("request failed").to_string());
        }
    }

    /// Every browser tab in the session as (tab_id, url), across all
    /// workspaces and screens.
    fn browser_tabs(&mut self) -> Result<Vec<(String, String)>, String> {
        let tree = self.request(json!({ "cmd": "list-workspaces" }))?;
        let mut tabs = Vec::new();
        let tab_lists = tree
            .get("workspaces")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|ws| ws.get("screens").and_then(Value::as_array).into_iter().flatten())
            .flat_map(|screen| screen.get("panes").and_then(Value::as_array).into_iter().flatten())
            .flat_map(|pane| pane.get("tabs").and_then(Value::as_array).into_iter().flatten());
        for tab in tab_lists {
            if tab.get("kind").and_then(Value::as_str) != Some("browser") {
                continue;
            }
            let Some(id) = tab.get("tab_resource_id").and_then(Value::as_str) else { continue };
            let url = tab.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()).unwrap_or("about:blank");
            tabs.push((id.to_string(), url.to_string()));
        }
        Ok(tabs)
    }
}

/// The browser binary: `CMUX_LIN_BROWSER`, else the first Chromium-family
/// browser on PATH. Open-source Chromium comes first; Google Chrome (which
/// reports usage to Google even when headless) is the last resort.
fn browser_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CMUX_LIN_BROWSER") {
        return Some(PathBuf::from(path));
    }
    let names = [
        "chromium",
        "chromium-browser",
        "ungoogled-chromium",
        "brave-browser",
        "brave",
        "microsoft-edge",
        "google-chrome-stable",
        "google-chrome",
    ];
    let path = std::env::var_os("PATH")?;
    names
        .iter()
        .flat_map(|name| std::env::split_paths(&path).map(move |dir| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// One profile per browser: Chromium builds of different versions can't
/// safely share a profile (a newer one upgrades it past what an older reads).
fn profile_dir(browser: &str) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cmux-lin/browsers")
        .join(browser)
}

fn launch_chrome() -> Result<Chrome, String> {
    let binary = browser_binary().ok_or(
        "Browser tabs need a Chromium-based browser, and none was found. \
         Install Chromium (e.g. `sudo dnf install chromium`) or set CMUX_LIN_BROWSER \
         to a Chromium-family browser.",
    )?;
    if !binary.is_file() {
        return Err(format!("CMUX_LIN_BROWSER points to {}, which doesn't exist.", binary.display()));
    }
    let name = binary.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let profile = profile_dir(&name);
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let port_file = profile.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&port_file);
    // Debugging pipe: the browser reads fd 3 and writes fd 4. We keep the
    // other ends; they're close-on-exec, so only we hold them.
    let (browser_reads, our_writer) = pipe()?;
    let (our_reader, browser_writes) = pipe()?;
    let mut command = Command::new(&binary);
    command
        .arg("--headless=new")
        .arg("--remote-debugging-pipe")
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--hide-scrollbars", "about:blank"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Chrome must not outlive the provider thread that started it (that
    // thread lives until the provider stops).
    // Signal masks are inherited across exec; start Chrome with none blocked
    // (the standalone binary blocks SIGINT/SIGTERM to handle them itself).
    let (child_in, child_out) = {
        use std::os::fd::AsRawFd;
        (browser_reads.as_raw_fd(), browser_writes.as_raw_fd())
    };
    unsafe {
        command.pre_exec(move || {
            // Put the browser's pipe ends on fds 3 and 4, inheritable. (Our
            // pipes were created before spawn's own, so they're the lowest
            // descriptors and these moves can't clobber anything of std's.)
            for (from, to) in [(child_in, 3), (child_out, 4)] {
                if from == to {
                    let flags = libc::fcntl(to, libc::F_GETFD);
                    libc::fcntl(to, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                } else if libc::dup2(from, to) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let child = command.spawn().map_err(|e| format!("{}: {e}", binary.display()))?;
    drop((browser_reads, browser_writes)); // the browser has its copies
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut chrome = Chrome { child, _pipe: [our_writer, our_reader], name, port: 0, endpoint: String::new() };
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&port_file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if let Ok(port) = port.trim().parse::<u16>() {
                    chrome.port = port;
                    chrome.endpoint = format!("ws://127.0.0.1:{port}{}", path.trim());
                    return Ok(chrome);
                }
            }
        }
        if !matches!(chrome.child.try_wait(), Ok(None)) {
            return Err(format!("{} exited during startup.", binary.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("{} did not report its DevTools port.", binary.display()))
}

/// Open a page target at `url`; returns its target id.
fn open_page(port: u16, url: &str) -> Result<String, String> {
    let body = http(port, "PUT", &format!("/json/new?{}", encode_query(url)))?;
    let page: Value = serde_json::from_str(&body).map_err(|e| format!("bad DevTools reply: {e}"))?;
    page.get("id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| "DevTools reply had no id".into())
}

/// Percent-encode what HTTP would otherwise cut or reject in a query.
fn encode_query(url: &str) -> String {
    url.bytes()
        .map(|b| match b {
            b' ' | b'#' | b'"' | b'<' | b'>' | b'%' | 0x00..=0x1f | 0x7f..=0xff => format!("%{b:02X}"),
            _ => (b as char).to_string(),
        })
        .collect()
}

/// Minimal HTTP/1.1 request to Chrome's loopback DevTools server. Chrome
/// keeps the socket open despite `Connection: close`, so the body is read by
/// `Content-Length`, not to end of stream.
fn http(port: u16, method: &str, path: &str) -> Result<String, String> {
    let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    (&stream).write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).map_err(|e| e.to_string())?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if !status.starts_with("HTTP/1.1 200") {
        return Err(status.trim().to_string());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    String::from_utf8(body).map_err(|e| e.to_string())
}

/// The text selected in browser tab `tab_id` of the session at
/// `socket_path`: the selected part of a focused text field, else the page
/// selection. Empty when nothing is selected.
///
/// The session protocol has no selection command for browser tabs (`copy`
/// is terminal-only), so this asks the page itself over the DevTools
/// Protocol, using the endpoint and target the provider registered.
pub fn selected_text(socket_path: &Path, tab_id: &str) -> Result<String, String> {
    const EXPRESSION: &str = r#"(() => {
        const el = document.activeElement;
        const text = el && (el.tagName === "TEXTAREA" ||
            (el.tagName === "INPUT" && /^(text|search|url|tel|email|number)$/i.test(el.type)));
        if (text && el.selectionStart !== el.selectionEnd)
            return el.value.substring(el.selectionStart, el.selectionEnd);
        return String(window.getSelection() || "");
    })()"#;
    let value = evaluate(socket_path, tab_id, EXPRESSION)?;
    Ok(value.as_str().unwrap_or("").to_string())
}

/// The link (absolute URL) under page point (`x`, `y`) in CSS pixels in
/// browser tab `tab_id`, if there is one.
pub fn link_at(socket_path: &Path, tab_id: &str, x: f64, y: f64) -> Result<Option<String>, String> {
    let expression = format!(
        "(() => {{ const e = document.elementFromPoint({x}, {y}); \
         const a = e && e.closest('a[href]'); return a ? a.href : null; }})()"
    );
    let value = evaluate(socket_path, tab_id, &expression)?;
    Ok(value.as_str().filter(|href| !href.is_empty()).map(str::to_string))
}

/// A DevTools WebSocket on a page.
type PageSocket = tungstenite::WebSocket<TcpStream>;

/// Evaluate `expression` in the page of browser tab `tab_id` and return its
/// value (by value, JSON).
pub fn evaluate(socket_path: &Path, tab_id: &str, expression: &str) -> Result<Value, String> {
    let mut socket = page_socket(socket_path, tab_id)?;
    let request = json!({
        "id": 1,
        "method": "Runtime.evaluate",
        "params": { "expression": expression, "returnByValue": true },
    });
    socket.send(tungstenite::Message::text(request.to_string())).map_err(|e| e.to_string())?;
    loop {
        let message = socket.read().map_err(|e| e.to_string())?;
        let Ok(text) = message.to_text() else { continue };
        let Ok(reply) = serde_json::from_str::<Value>(text) else { continue };
        if reply.get("id").and_then(Value::as_u64) != Some(1) {
            continue;
        }
        let _ = socket.close(None);
        if let Some(error) = reply.get("error") {
            return Err(error.get("message").and_then(Value::as_str).unwrap_or("evaluation failed").to_string());
        }
        return Ok(reply.pointer("/result/result/value").cloned().unwrap_or(Value::Null));
    }
}

/// Open a DevTools connection to the page of browser tab `tab_id`, using the
/// endpoint and target the session's provider registered. Loopback only.
fn page_socket(socket_path: &Path, tab_id: &str) -> Result<PageSocket, String> {
    let mut session = Session::connect(socket_path).map_err(|e| e.to_string())?;
    let provider = session.request(json!({ "cmd": "get-browser-provider" }))?;
    if provider.get("available") != Some(&Value::Bool(true)) {
        return Err("no browser provider is running".into());
    }
    if provider.get("authentication").and_then(Value::as_str) == Some("bearer") {
        return Err("the browser provider requires authentication".into());
    }
    let target = provider
        .get("targets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|t| t.get("tab_id").and_then(Value::as_str) == Some(tab_id))
        .and_then(|t| t.get("target_id").and_then(Value::as_str))
        .ok_or("this tab has no page yet")?
        .to_string();
    // ws://127.0.0.1:PORT/devtools/browser/ID → the page's own endpoint.
    let endpoint = provider.get("endpoint").and_then(Value::as_str).ok_or("provider has no endpoint")?;
    let authority = endpoint
        .strip_prefix("ws://")
        .and_then(|rest| rest.split('/').next())
        .ok_or("unexpected provider endpoint")?;
    let host_port: Vec<&str> = authority.rsplitn(2, ':').collect();
    let (port, host) = match host_port.as_slice() {
        [port, host] => (port.parse::<u16>().map_err(|_| "bad endpoint port")?, *host),
        _ => return Err("unexpected provider endpoint".into()),
    };
    if host != "127.0.0.1" && host != "localhost" && host != "[::1]" {
        return Err("refusing a non-loopback DevTools endpoint".into());
    }
    let url = format!("ws://{authority}/devtools/page/{target}");
    let stream = TcpStream::connect((host.trim_matches(|c| c == '[' || c == ']'), port)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let (socket, _) = tungstenite::client(url.as_str(), stream).map_err(|e| e.to_string())?;
    Ok(socket)
}

/// A page asked for a file (its `<input type=file>` was activated).
#[derive(Debug, Clone)]
pub struct FileChooser {
    /// The input element, for [`FileChooserWatch::set_files`].
    pub node: i64,
    /// Whether the input accepts several files.
    pub multiple: bool,
}

/// Intercepts file pickers in one browser tab. A headless browser can't show
/// a file dialog, so the page reports the request instead and the host
/// answers it with [`set_files`](Self::set_files). Dropping it stops
/// watching (pickers are then ignored again).
pub struct FileChooserWatch {
    files: mpsc::Sender<(i64, Vec<String>)>,
    stop: Arc<AtomicBool>,
}

impl FileChooserWatch {
    /// Give the page's file input `node` these local files (absolute paths).
    pub fn set_files(&self, node: i64, paths: Vec<String>) {
        let _ = self.files.send((node, paths));
    }
}

impl Drop for FileChooserWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Start intercepting file pickers in browser tab `tab_id`; `on_open` runs
/// (on the watcher's thread) for each request. Connects in the background
/// and retries until the tab has a page, so it can start with the tab.
pub fn watch_file_choosers(
    socket_path: &Path,
    tab_id: &str,
    on_open: impl Fn(FileChooser) + Send + 'static,
) -> FileChooserWatch {
    let (files, requests) = mpsc::channel::<(i64, Vec<String>)>();
    let stop = Arc::new(AtomicBool::new(false));
    let (socket_path, tab_id, thread_stop) = (socket_path.to_path_buf(), tab_id.to_string(), stop.clone());
    std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            if let Ok(socket) = page_socket(&socket_path, &tab_id) {
                watch(socket, &requests, &thread_stop, &on_open);
            }
            sleep(&thread_stop, Duration::from_secs(1));
        }
    });
    FileChooserWatch { files, stop }
}

/// Serve one page connection until it closes or we're stopped.
fn watch(
    mut socket: PageSocket,
    requests: &mpsc::Receiver<(i64, Vec<String>)>,
    stop: &AtomicBool,
    on_open: &dyn Fn(FileChooser),
) {
    // Short reads, so queued answers go out promptly.
    let _ = socket.get_ref().set_read_timeout(Some(Duration::from_millis(200)));
    let mut next_id = 1u64;
    let mut call = |socket: &mut PageSocket, method: &str, params: Value| {
        let message = json!({ "id": next_id, "method": method, "params": params });
        next_id += 1;
        socket.send(tungstenite::Message::text(message.to_string())).is_ok()
    };
    if !call(&mut socket, "Page.enable", json!({}))
        || !call(&mut socket, "Page.setInterceptFileChooserDialog", json!({ "enabled": true }))
    {
        return;
    }
    while !stop.load(Ordering::Acquire) {
        while let Ok((node, files)) = requests.try_recv() {
            if !call(&mut socket, "DOM.setFileInputFiles", json!({ "backendNodeId": node, "files": files })) {
                return;
            }
        }
        match socket.read() {
            Ok(message) => {
                let Ok(text) = message.to_text() else { continue };
                let Ok(event) = serde_json::from_str::<Value>(text) else { continue };
                if event.get("method").and_then(Value::as_str) == Some("Page.fileChooserOpened") {
                    let params = event.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(node) = params.get("backendNodeId").and_then(Value::as_i64) {
                        on_open(FileChooser {
                            node,
                            multiple: params.get("mode").and_then(Value::as_str) == Some("selectMultiple"),
                        });
                    }
                }
            }
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => return, // page or browser went away; the caller reconnects
        }
    }
    let _ = socket.close(None);
}
