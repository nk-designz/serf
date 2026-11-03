//! The `serf-msg` control channel.
//!
//! serf listens on a UNIX domain socket (default `/tmp/serf.sock`). Clients
//! send one **command per line**; serf answers with one **JSON object per
//! line**. This is what lets external tools drive the browser:
//!
//! ```text
//! $ serf-msg open https://example.com
//! {"ok":true,"message":"navigating"}
//! $ serf-msg status
//! {"ok":true,"message":"https://example.com — normal","data":{"url":"…"}}
//! ```
//!
//! The socket is created mode `0600` so other users on the machine cannot
//! drive *your* browser, and a stale socket left by a crash is reclaimed
//! automatically on bind.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

/// Default socket path (mirrors [`crate::config::DEFAULT_SOCKET`]).
pub const DEFAULT_SOCKET: &str = "/tmp/serf.sock";

/// A reply sent back to a `serf-msg` client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            data: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn to_line(&self) -> String {
        // Serialising a `Response` cannot fail (no maps with non-string keys,
        // no NaN); fall back to a hand-built error line just in case.
        serde_json::to_string(self).unwrap_or_else(|e| {
            format!(r#"{{"ok":false,"message":"serialisation failed: {e}"}}"#)
        })
    }
}

/// The listening half of the control channel.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

impl Server {
    /// Bind the socket, reclaiming a stale one if the previous owner is gone.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        if path.exists() {
            // If we can connect, someone is already serving here.
            if UnixStream::connect(path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("another serf is already listening on {}", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        // Owner-only: this socket can navigate, scroll and quit the browser.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serve forever, dispatching each received line to `handler`.
    ///
    /// Returns the thread handle; the thread exits when the listener errors.
    pub fn serve<F>(self, mut handler: F) -> JoinHandle<()>
    where
        F: FnMut(&str) -> Response + Send + 'static,
    {
        std::thread::spawn(move || {
            for stream in self.listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("serf-msg: accept failed: {e}");
                        continue;
                    }
                };
                let reader = BufReader::new(match stream.try_clone() {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("serf-msg: clone failed: {e}");
                        continue;
                    }
                });
                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    let cmd = line.trim();
                    if cmd.is_empty() {
                        continue;
                    }
                    let response = handler(cmd);
                    if stream
                        .write_all(response.to_line().as_bytes())
                        .and_then(|_| stream.write_all(b"\n"))
                        .and_then(|_| stream.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Open a connection to a running serf.
pub fn connect(path: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(path)
}

/// Send one command and read one `Response`.
pub fn request(path: &Path, command: &str) -> std::io::Result<Response> {
    let mut stream = connect(path)?;
    stream.write_all(command.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    serde_json::from_str(line.trim()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad response from serf: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    fn tmp_sock(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("serf-ipc-{tag}-{}-{n}.sock", std::process::id()))
    }

    #[test]
    fn response_lines_are_json() {
        let r = Response::ok("hello");
        let line = r.to_line();
        let parsed: Response = serde_json::from_str(&line).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.message, "hello");
        assert!(parsed.data.is_none());
        // `data` is omitted when absent.
        assert!(!line.contains("data"));
    }

    #[test]
    fn response_carries_data() {
        let r = Response::ok("status")
            .with_data(serde_json::json!({"url": "https://x/", "mode": "normal"}));
        let parsed: Response = serde_json::from_str(&r.to_line()).unwrap();
        assert_eq!(parsed.data.unwrap()["mode"], "normal");
    }

    #[test]
    fn round_trips_a_command_over_the_socket() {
        let path = tmp_sock("roundtrip");
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);

        let server = Server::bind(&path).expect("bind");
        let handle = server.serve(move |cmd| {
            seen2.lock().unwrap().push(cmd.to_string());
            if cmd == "ping" {
                Response::ok("pong")
            } else {
                Response::err(format!("unknown: {cmd}"))
            }
        });

        let r = request(&path, "ping").expect("request");
        assert!(r.ok);
        assert_eq!(r.message, "pong");

        let r2 = request(&path, "bogus").expect("request 2");
        assert!(!r2.ok);
        assert_eq!(r2.message, "unknown: bogus");

        assert_eq!(&*seen.lock().unwrap(), &["ping".to_string(), "bogus".to_string()]);

        let _ = handle; // thread ends when the listener is dropped
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn several_commands_on_one_connection() {
        let path = tmp_sock("multi");
        let server = Server::bind(&path).expect("bind");
        let handle = server.serve(|cmd| Response::ok(format!("echo:{cmd}")));

        let mut stream = connect(&path).expect("connect");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        for word in ["a", "b", "c"] {
            stream.write_all(format!("{word}\n").as_bytes()).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let r: Response = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(r.message, format!("echo:{word}"));
        }
        let _ = handle;
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_stale_socket_is_reclaimed_on_bind() {
        let path = tmp_sock("stale");
        // Create a socket file with no listener behind it.
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener); // leaves the file behind, nobody listening
        assert!(path.exists());

        let server = Server::bind(&path);
        assert!(server.is_ok(), "stale socket should be reclaimed");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn refuses_to_double_bind_a_live_socket() {
        let path = tmp_sock("live");
        let _first = Server::bind(&path).expect("first bind");
        let second = Server::bind(&path);
        assert!(second.is_err(), "second bind must be refused");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn socket_is_owner_only() {
        let path = tmp_sock("perms");
        let _server = Server::bind(&path).expect("bind");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "socket must not be world-writable");
        std::fs::remove_file(&path).ok();
    }
}
