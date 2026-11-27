//! Import cookies from the person's other browsers into the provider's
//! browser, so browser tabs start out logged in (the macOS app's "browser
//! import").
//!
//! Sources are the profiles of Firefox and of Chromium-family browsers
//! (Chromium, Chrome, Brave, Edge, Vivaldi; Flatpak installs too). Each
//! cookie store is read from a temporary copy, so the browser can stay open.
//! Firefox keeps values in plain text. Chromium-family browsers encrypt
//! them (AES-128-CBC, IV of 16 spaces): `v10` values with a key derived from
//! the fixed password `peanuts`, `v11` values with a key derived from the
//! password the browser keeps in the desktop keyring, read with
//! `secret-tool` (the keyring may ask to be unlocked). Newer stores prefix
//! each value with a 32-byte hash of its domain, which is dropped.
//!
//! Cookies go into the running browser with the DevTools
//! `Storage.setCookies` command over the provider's loopback endpoint.
//! Nothing leaves the machine. Expired cookies are skipped.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::Session;

/// One browser profile to import from.
#[derive(Debug, Clone)]
pub struct Source {
    /// Shown in lists, e.g. `Brave (Personal)`.
    pub label: String,
    kind: Kind,
    /// The profile's cookie database.
    database: PathBuf,
}

#[derive(Debug, Clone)]
enum Kind {
    Firefox,
    /// With the keyring `application` names to try for `v11` values.
    Chromium(&'static [&'static str]),
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The browser profiles with a cookie store on this machine.
pub fn sources() -> Vec<Source> {
    let home = home();
    let mut out = Vec::new();
    let chromium: [(&str, &str, &'static [&'static str]); 8] = [
        (".config/chromium", "Chromium", &["chromium"]),
        (".config/google-chrome", "Chrome", &["chrome"]),
        (".config/BraveSoftware/Brave-Browser", "Brave", &["brave"]),
        (".config/microsoft-edge", "Edge", &["microsoft-edge", "chromium"]),
        (".config/vivaldi", "Vivaldi", &["vivaldi", "chrome"]),
        (".var/app/org.chromium.Chromium/config/chromium", "Chromium (Flatpak)", &["chromium"]),
        (".var/app/com.google.Chrome/config/google-chrome", "Chrome (Flatpak)", &["chrome"]),
        (".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser", "Brave (Flatpak)", &["brave"]),
    ];
    for (dir, browser, apps) in chromium {
        let base = home.join(dir);
        let names = profile_names(&base);
        let mut profiles: Vec<PathBuf> = std::fs::read_dir(&base)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "Default" || n.starts_with("Profile "))
            })
            .collect();
        profiles.sort();
        for profile in profiles {
            let database = [profile.join("Network/Cookies"), profile.join("Cookies")].into_iter().find(|p| p.is_file());
            let Some(database) = database else { continue };
            let dir_name = profile.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let shown = names.get(&dir_name).cloned().unwrap_or(dir_name);
            out.push(Source { label: format!("{browser} ({shown})"), kind: Kind::Chromium(apps), database });
        }
    }
    for (dir, browser) in [(".mozilla/firefox", "Firefox"), (".var/app/org.mozilla.firefox/.mozilla/firefox", "Firefox (Flatpak)")] {
        let base = home.join(dir);
        let Ok(ini) = std::fs::read_to_string(base.join("profiles.ini")) else { continue };
        let (mut name, mut path, mut relative) = (None::<String>, None::<String>, true);
        let mut flush = |name: &mut Option<String>, path: &mut Option<String>, relative: bool| {
            if let Some(p) = path.take() {
                let full = if relative { base.join(&p) } else { PathBuf::from(&p) };
                let database = full.join("cookies.sqlite");
                if database.is_file() {
                    let shown = name.take().unwrap_or(p);
                    out.push(Source { label: format!("{browser} ({shown})"), kind: Kind::Firefox, database });
                }
            }
            *name = None;
        };
        for line in ini.lines().map(str::trim) {
            if line.starts_with('[') {
                flush(&mut name, &mut path, relative);
                relative = true;
            } else if let Some(v) = line.strip_prefix("Name=") {
                name = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("Path=") {
                path = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("IsRelative=") {
                relative = v != "0";
            }
        }
        flush(&mut name, &mut path, relative);
    }
    out
}

/// Chromium profile display names from `Local State`, by profile folder.
fn profile_names(base: &Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(base.join("Local State"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.pointer("/profile/info_cache").and_then(Value::as_object).cloned())
        .map(|cache| {
            cache
                .into_iter()
                .filter_map(|(dir, info)| Some((dir, info.get("name")?.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Import every unexpired cookie of `source` into the session's provider
/// browser. Returns how many were imported and how many couldn't be
/// decrypted.
pub fn import(socket_path: &Path, source: &Source) -> Result<(usize, usize), String> {
    let (cookies, undecryptable) = read(source)?;
    if cookies.is_empty() {
        return Ok((0, undecryptable));
    }
    let mut socket = browser_socket(socket_path)?;
    let mut id = 0;
    for chunk in cookies.chunks(500) {
        id += 1;
        let request = json!({ "id": id, "method": "Storage.setCookies", "params": { "cookies": chunk } });
        socket.send(tungstenite::Message::text(request.to_string())).map_err(|e| e.to_string())?;
        loop {
            let message = socket.read().map_err(|e| e.to_string())?;
            let Ok(text) = message.to_text() else { continue };
            let Ok(reply) = serde_json::from_str::<Value>(text) else { continue };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(error.get("message").and_then(Value::as_str).unwrap_or("Storage.setCookies failed").to_string());
            }
            break;
        }
    }
    let _ = socket.close(None);
    Ok((cookies.len(), undecryptable))
}

/// How many unexpired cookies `source` has that can be read, and how many
/// can't be decrypted (without importing anything).
pub fn inspect(source: &Source) -> Result<(usize, usize), String> {
    read(source).map(|(cookies, failed)| (cookies.len(), failed))
}

/// The cookies as DevTools `CookieParam`s, and how many couldn't be read.
fn read(source: &Source) -> Result<(Vec<Value>, usize), String> {
    let copy = TempCopy::of(&source.database)?;
    let db = rusqlite::Connection::open_with_flags(&copy.database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("can't open the cookie store: {e}"))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    match source.kind {
        Kind::Firefox => read_firefox(&db, now),
        Kind::Chromium(apps) => read_chromium(&db, now, apps),
    }
}

fn read_firefox(db: &rusqlite::Connection, now: i64) -> Result<(Vec<Value>, usize), String> {
    let mut statement = db
        .prepare("SELECT host, name, value, path, expiry, isSecure, isHttpOnly, sameSite FROM moz_cookies")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (host, name, value, path, expiry, secure, http_only, same_site) = row;
        // Newer Firefox stores milliseconds.
        let expires = if expiry > 100_000_000_000 { expiry / 1000 } else { expiry };
        if expires != 0 && expires < now {
            continue;
        }
        let same_site = match same_site {
            0 => Some("None"),
            1 => Some("Lax"),
            2 => Some("Strict"),
            _ => None,
        };
        out.push(cookie_param(&host, &name, &value, &path, (expires > 0).then_some(expires), secure != 0, http_only != 0, same_site));
    }
    Ok((out, 0))
}

fn read_chromium(db: &rusqlite::Connection, now: i64, apps: &[&str]) -> Result<(Vec<Value>, usize), String> {
    // Store version 24 and later prefix each value with SHA-256(domain).
    let version: i64 = db
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let v10_key = derive_key(b"peanuts");
    let mut v11_key: Option<Option<[u8; 16]>> = None;
    let mut statement = db
        .prepare(
            "SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly, samesite FROM cookies",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut failed = 0;
    for row in rows.flatten() {
        let (host, name, plain, encrypted, path, expires_utc, secure, http_only, same_site) = row;
        // Microseconds since 1601-01-01; 0 for a session cookie.
        let expires = if expires_utc > 0 { expires_utc / 1_000_000 - 11_644_473_600 } else { 0 };
        if expires != 0 && expires < now {
            continue;
        }
        let value = if !plain.is_empty() || encrypted.is_empty() {
            Some(plain)
        } else {
            let key = match encrypted.get(..3) {
                Some(b"v10") => Some(v10_key),
                Some(b"v11") => *v11_key.get_or_insert_with(|| keyring_key(apps)),
                _ => None,
            };
            key.and_then(|key| decrypt(&key, &encrypted[3..])).and_then(|bytes| {
                let bytes = if version >= 24 && bytes.len() >= 32 { bytes[32..].to_vec() } else { bytes };
                String::from_utf8(bytes).ok()
            })
        };
        let Some(value) = value else {
            failed += 1;
            continue;
        };
        let same_site = match same_site {
            0 => Some("None"),
            1 => Some("Lax"),
            2 => Some("Strict"),
            _ => None,
        };
        out.push(cookie_param(&host, &name, &value, &path, (expires > 0).then_some(expires), secure != 0, http_only != 0, same_site));
    }
    Ok((out, failed))
}

/// A DevTools `CookieParam`. A host-only cookie (no leading dot) is set by
/// URL so it stays host-only; a domain cookie keeps its domain.
#[allow(clippy::too_many_arguments)]
fn cookie_param(host: &str, name: &str, value: &str, path: &str, expires: Option<i64>, secure: bool, http_only: bool, same_site: Option<&str>) -> Value {
    let mut cookie = json!({
        "name": name,
        "value": value,
        "path": if path.is_empty() { "/" } else { path },
        "secure": secure,
        "httpOnly": http_only,
    });
    if host.starts_with('.') {
        cookie["domain"] = json!(host);
    } else {
        let scheme = if secure { "https" } else { "http" };
        cookie["url"] = json!(format!("{scheme}://{host}{}", if path.is_empty() { "/" } else { path }));
    }
    if let Some(expires) = expires {
        cookie["expires"] = json!(expires);
    }
    if let Some(same_site) = same_site {
        cookie["sameSite"] = json!(same_site);
    }
    cookie
}

/// Chromium's cookie key: PBKDF2-HMAC-SHA1(password, "saltysalt", 1, 16).
fn derive_key(password: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1, &mut key);
    key
}

/// The `v11` key from the browser's keyring password (`secret-tool lookup
/// application <name>`), trying each name. `None` without a keyring entry.
fn keyring_key(apps: &[&str]) -> Option<[u8; 16]> {
    for app in apps {
        let output = std::process::Command::new("secret-tool")
            .args(["lookup", "application", app])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let password = String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_string();
        if output.status.success() && !password.is_empty() {
            return Some(derive_key(password.as_bytes()));
        }
    }
    None
}

/// AES-128-CBC with an IV of 16 spaces and PKCS#7 padding.
fn decrypt(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let iv = [b' '; 16];
    let mut buffer = data.to_vec();
    let plain = cbc::Decryptor::<aes::Aes128>::new(key.into(), &iv.into()).decrypt_padded_mut::<Pkcs7>(&mut buffer).ok()?;
    Some(plain.to_vec())
}

/// A private copy of a SQLite database and its journal files, removed when
/// dropped.
struct TempCopy {
    dir: PathBuf,
    database: PathBuf,
}

impl TempCopy {
    fn of(database: &Path) -> Result<Self, String> {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = base.join(format!("cmux-lin-cookies-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // Owner-only: these are someone's logins.
        let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let copy = Self { database: dir.join("cookies.db"), dir };
        std::fs::copy(database, &copy.database).map_err(|e| format!("can't read the cookie store: {e}"))?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut from = database.as_os_str().to_owned();
            from.push(suffix);
            let mut to = copy.database.as_os_str().to_owned();
            to.push(suffix);
            let _ = std::fs::copy(PathBuf::from(from), PathBuf::from(to));
        }
        Ok(copy)
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A DevTools connection to the provider's browser itself (not a page).
fn browser_socket(socket_path: &Path) -> Result<tungstenite::WebSocket<TcpStream>, String> {
    let mut session = Session::connect(socket_path).map_err(|e| e.to_string())?;
    let provider = session.request(json!({ "cmd": "get-browser-provider" }))?;
    if provider.get("available") != Some(&Value::Bool(true)) {
        return Err("no browser provider is running".into());
    }
    let endpoint = provider
        .get("endpoint")
        .and_then(Value::as_str)
        .filter(|e| e.starts_with("ws://"))
        .ok_or("the browser hasn't started yet: open a browser tab first")?
        .to_string();
    let authority = endpoint.trim_start_matches("ws://").split('/').next().unwrap_or("").to_string();
    let (host, port) = authority.rsplit_once(':').ok_or("unexpected provider endpoint")?;
    if host != "127.0.0.1" && host != "localhost" {
        return Err("refusing a non-loopback DevTools endpoint".into());
    }
    let port: u16 = port.parse().map_err(|_| "bad endpoint port")?;
    let stream = TcpStream::connect((host, port)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(20))).map_err(|e| e.to_string())?;
    let (socket, _) = tungstenite::client(endpoint.as_str(), stream).map_err(|e| e.to_string())?;
    Ok(socket)
}
