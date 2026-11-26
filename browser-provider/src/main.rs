//! Standalone Linux browser provider for a cmux session, so browser tabs
//! work in the TUI (and any other client) without the GTK app.
//!
//! Usage: cmux-browser-provider [--session NAME]   (default: main)
//!
//! Runs until SIGINT/SIGTERM, then unregisters and closes its Chrome.

use cmux_browser_provider as provider;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut session = "main".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => session = args.next().unwrap_or(session),
            other if other.starts_with("--session=") => session = other["--session=".len()..].to_string(),
            "-h" | "--help" => {
                println!("usage: cmux-browser-provider [--session NAME]");
                return;
            }
            other => {
                eprintln!("cmux-browser-provider: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let socket = match cmux::raw::ClientConfig::try_from_env_or_default_session(&session) {
        Ok(config) => config.socket_path,
        Err(error) => {
            eprintln!("cmux-browser-provider: {error}");
            std::process::exit(1);
        }
    };

    // Block SIGINT/SIGTERM before any thread starts, then wait for one here,
    // so shutdown always runs the provider's cleanup.
    let mut signals: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut signals);
        libc::sigaddset(&mut signals, libc::SIGINT);
        libc::sigaddset(&mut signals, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
    }

    eprintln!("cmux-browser-provider: serving browser tabs for session {session}");
    let handle = provider::start(socket, |report| match report {
        provider::Report::Info(line) => eprintln!("{line}"),
        provider::Report::Problem(problem) => eprintln!("cmux-browser-provider: {problem} (retrying every 30 s)"),
        provider::Report::Resolved => eprintln!("cmux-browser-provider: browser started"),
    });
    let mut received = 0;
    unsafe {
        libc::sigwait(&signals, &mut received);
    }
    eprintln!("cmux-browser-provider: stopping");
    drop(handle);
}
