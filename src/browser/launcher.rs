//! The external menu launcher.
//!
//! serf has no built-in chrome, so every chooser — the tab switcher, a
//! `<select>` element, a pick-one prompt — is piped to whatever `tab_launcher`
//! the config names (`fuzzel --dmenu`, `wofi --dmenu`, `dmenu -i`, ...).
//!
//! The command is run through `sh -c`, so a config value may carry arguments and
//! pipes exactly as the user would type them at a shell. That is the same
//! contract surf gives its `dmenu` command, and it keeps serf from growing an
//! argument splitter that gets quoting subtly wrong.
//!
//! A menu is interactive, so it must never block the winit event loop: the child
//! runs on its own thread, and the chosen line comes back through a channel that
//! the event loop polls on each turn.

use std::io::{Read, Write};
use std::process::{Command as StdCommand, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

/// A running external menu. Dropping it abandons the menu: the child is reaped
/// by its own thread whenever it exits.
pub struct Launcher {
    /// The line the user picked, once the child exits.
    rx: Receiver<Option<String>>,
    /// Kept for logging — which command produced this menu.
    command: String,
}

impl Launcher {
    /// Spawn `command`, feed it `entries` on stdin, and start reading the choice
    /// on a background thread.
    ///
    /// `entries` is written verbatim; callers append their own newlines (see
    /// [`Session::launcher_entries`](crate::browser::navigation::Session::launcher_entries)).
    pub fn spawn(command: &str, entries: &str) -> std::io::Result<Launcher> {
        let mut child = StdCommand::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let entries = entries.to_string();

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            // Feed the menu, then close stdin: a dmenu-alike shows nothing until
            // it sees EOF. A write error means the launcher exited without
            // reading (Escape before drawing); that is a cancel, not a failure,
            // so fall through to the reap instead of returning early.
            let _ = stdin.write_all(entries.as_bytes());
            let _ = stdin.write_all(b"\n");
            drop(stdin);

            let mut out = String::new();
            let _ = stdout.read_to_string(&mut out);
            let _ = child.wait();
            let _ = tx.send(first_nonempty(&out));
        });

        Ok(Launcher {
            rx,
            command: command.to_string(),
        })
    }

    /// The command that produced this menu.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The chosen line, or `None` while the menu is still open.
    ///
    /// The outer `Option` is "has the menu closed?"; the inner one is the
    /// choice. Cancelling (Escape, or a launcher that prints nothing) shows up
    /// as `Some(None)`, which is distinct from "still open".
    pub fn poll(&self) -> Option<Option<String>> {
        match self.rx.try_recv() {
            Ok(line) => Some(line),
            Err(TryRecvError::Empty) => None,
            // The thread died without sending: treat as a cancel so the caller
            // cannot get stuck waiting on a menu that will never answer.
            Err(TryRecvError::Disconnected) => Some(None),
        }
    }
}

/// The first non-blank line of the launcher's output.
///
/// Older `dmenu` builds and terminal-based pickers can emit a trailing newline
/// or a blank line; the chooser's answer is the first line with content.
pub fn first_nonempty(out: &str) -> Option<String> {
    out.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Block until the menu settles, so the tests do not race the child.
    fn settle(launcher: &Launcher) -> Option<Option<String>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(choice) = launcher.poll() {
                return Some(choice);
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn first_nonempty_skips_blanks() {
        assert_eq!(first_nonempty("\n\n  2: Gamma  \n"), Some("2: Gamma".into()));
        assert_eq!(first_nonempty("1: Beta\n2: Gamma\n"), Some("1: Beta".into()));
        assert_eq!(first_nonempty(""), None);
        assert_eq!(first_nonempty("   \n\n"), None);
    }

    #[test]
    fn a_choice_comes_back_over_the_channel() {
        // `head -1` is a launcher that always picks the first entry, so the
        // whole spawn/feed/read path is exercised without needing fuzzel.
        let entries = "0: Alpha\n1: Beta\n2: Gamma";
        let launcher = Launcher::spawn("head -1", entries).expect("spawn head");
        let choice = settle(&launcher).expect("menu should have closed");
        assert_eq!(choice.as_deref(), Some("0: Alpha"));
    }

    #[test]
    fn a_silent_launcher_is_a_cancel_not_a_hang() {
        // `true` exits without reading stdin or printing anything — an Escape.
        let launcher = Launcher::spawn("true", "0: Alpha\n1: Beta").expect("spawn true");
        let choice = settle(&launcher).expect("menu should have closed");
        assert_eq!(choice, None);
    }

    #[test]
    fn the_choice_parses_through_parse_launcher_choice() {
        use crate::browser::navigation::Session;
        let session = Session::new("about:blank");
        let launcher = Launcher::spawn("tail -1", &session.launcher_entries()).expect("spawn tail");
        let choice = settle(&launcher).expect("menu should have closed");
        let line = choice.expect("one tab, so one entry");
        assert_eq!(session.parse_launcher_choice(&line), Some(0));
    }
}
