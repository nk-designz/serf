//! `serf-msg` — the client for serf's control socket.
//!
//! ```text
//! serf-msg open https://example.com
//! serf-msg tab-new
//! serf-msg theme reload
//! serf-msg status
//! ```
//!
//! One command per invocation; the reply is printed (the human-readable
//! message, plus the JSON `data` block when the command returns one). Exit
//! status is 0 on success and 1 when serf reports a failure, so scripts can
//! branch on it.

use std::path::PathBuf;
use std::process::ExitCode;

use serf::ipc;

fn main() -> ExitCode {
    let mut socket: Option<PathBuf> = std::env::var_os("SERF_SOCKET").map(PathBuf::from);
    let mut words: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                usage();
                return ExitCode::SUCCESS;
            }
            "-s" | "--socket" => match args.next() {
                Some(p) => socket = Some(PathBuf::from(p)),
                None => {
                    eprintln!("serf-msg: --socket needs a path");
                    return ExitCode::from(2);
                }
            },
            other => words.push(other.to_string()),
        }
    }

    if words.is_empty() {
        usage();
        return ExitCode::from(2);
    }

    let socket = socket.unwrap_or_else(|| PathBuf::from(serf::config::DEFAULT_SOCKET));
    let command = words.join(" ");

    match ipc::request(&socket, &command) {
        Ok(response) => {
            println!("{}", response.message);
            if let Some(data) = &response.data {
                match serde_json::to_string_pretty(data) {
                    Ok(pretty) => println!("{pretty}"),
                    Err(_) => println!("{data}"),
                }
            }
            if response.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            // 78 = EX_CONFIG is not portable here; use 2 for "cannot reach".
            eprintln!("serf-msg: {}: {e}", socket.display());
            ExitCode::from(2)
        }
    }
}

fn usage() {
    eprintln!(
        "serf-msg {} — send a command to a running serf\n\
         \n\
         USAGE: serf-msg [-s SOCKET] <command> [args]\n\
         \n\
         COMMANDS:\n\
         \x20 open URL | tab-new URL | tab-close | tab-select N | tab-next | tab-prev\n\
         \x20 back | forward | reload | scroll N | scroll-top | scroll-bottom\n\
         \x20 hint | mode {{normal,insert,command}} | theme reload | status | quit\n\
         \n\
         ENV: SERF_SOCKET overrides the default socket ({})",
        env!("CARGO_PKG_VERSION"),
        serf::config::DEFAULT_SOCKET
    );
}
