//! Tabs, per-tab history and the `serf-msg` command dispatch.
//!
//! This module is deliberately engine-agnostic: it is the *model* the engine
//! backend is driven from. Keeping it free of Servo types is what lets the
//! whole thing be unit-tested headlessly, and lets the tab launcher and the
//! IPC channel share one source of truth.

use serde::{Deserialize, Serialize};

use crate::ipc::Response;

/// Where a tab sits in its history stack.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub title: String,
    /// History entries; `index` points at the current one.
    pub history: Vec<String>,
    pub index: usize,
}

impl Tab {
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            title: url.clone(),
            history: vec![url],
            index: 0,
        }
    }

    pub fn current(&self) -> &str {
        self.history.get(self.index).map(String::as_str).unwrap_or("")
    }

    /// Record a new navigation, truncating any forward history.
    pub fn visit(&mut self, url: impl Into<String>) {
        let url = url.into();
        if self.current() == url {
            return;
        }
        self.history.truncate(self.index + 1);
        self.history.push(url);
        self.index = self.history.len() - 1;
    }

    /// Replace the current entry in place (reload after a redirect).
    pub fn replace(&mut self, url: impl Into<String>) {
        let url = url.into();
        if self.history.is_empty() {
            self.history.push(url);
            self.index = 0;
        } else {
            self.history[self.index] = url;
        }
    }

    pub fn back(&mut self) -> Option<&str> {
        if self.index > 0 {
            self.index -= 1;
            Some(self.current())
        } else {
            None
        }
    }

    pub fn forward(&mut self) -> Option<&str> {
        if self.index + 1 < self.history.len() {
            self.index += 1;
            Some(self.current())
        } else {
            None
        }
    }

    pub fn can_go_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.index + 1 < self.history.len()
    }
}

/// The whole browser session: a list of tabs and which one is active.
#[derive(Debug, Clone)]
pub struct Session {
    tabs: Vec<Tab>,
    active: usize,
}

impl Default for Session {
    fn default() -> Self {
        Self::new("about:blank")
    }
}

impl Session {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            tabs: vec![Tab::new(url)],
            active: 0,
        }
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn current(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn current_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Open a new tab, make it active, and return its index.
    pub fn new_tab(&mut self, url: impl Into<String>) -> usize {
        self.tabs.push(Tab::new(url));
        self.active = self.tabs.len() - 1;
        self.active
    }

    /// Close the active tab. Closing the last tab is refused (there is always
    /// a window, surf-style). Returns the new active index.
    pub fn close_active(&mut self) -> Option<usize> {
        if self.tabs.len() <= 1 {
            return None;
        }
        self.tabs.remove(self.active);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        Some(self.active)
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.active = index;
            true
        } else {
            false
        }
    }

    /// Move to the next tab, wrapping.
    pub fn next_tab(&mut self) -> usize {
        self.active = (self.active + 1) % self.tabs.len();
        self.active
    }

    /// Move to the previous tab, wrapping.
    pub fn prev_tab(&mut self) -> usize {
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.active
    }

    /// A `fuzzel --dmenu`-friendly listing (`index: title` per line).
    pub fn launcher_entries(&self) -> String {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{i}: {}", t.title))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Parse the index chosen from [`Session::launcher_entries`].
    pub fn parse_launcher_choice(&self, line: &str) -> Option<usize> {
        line.split(':').next()?.trim().parse::<usize>().ok()
    }
}

/// The parsed form of a `serf-msg` command line.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Open(String),
    OpenNewTab(String),
    Close,
    Select(usize),
    Next,
    Prev,
    Back,
    Forward,
    Reload,
    Scroll { dx: f64, dy: f64 },
    ScrollTop,
    ScrollBottom,
    Hint,
    SetMode(String),
    Theme,
    Status,
    Quit,
}

impl Command {
    /// Parse `open https://x` / `tab-select 2` / `theme reload` …
    pub fn parse(line: &str) -> Result<Command, String> {
        let mut parts = line.split_whitespace();
        let head = parts.next().ok_or_else(|| "empty command".to_string())?;
        let arg = || parts.clone().collect::<Vec<_>>().join(" ");
        Ok(match head {
            "open" => Command::Open(arg()),
            "open-new" | "tab-new" => Command::OpenNewTab(arg()),
            "close" | "tab-close" => Command::Close,
            "tab-select" | "select" => {
                let n = parts
                    .next()
                    .ok_or("tab-select needs an index")?
                    .parse::<usize>()
                    .map_err(|_| "tab index must be a number".to_string())?;
                Command::Select(n)
            }
            "tab-next" | "next" => Command::Next,
            "tab-prev" | "prev" => Command::Prev,
            "back" => Command::Back,
            "forward" => Command::Forward,
            "reload" => Command::Reload,
            "scroll" => {
                let dy = parts
                    .next()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(80.0);
                Command::Scroll { dx: 0.0, dy }
            }
            "scroll-top" | "top" => Command::ScrollTop,
            "scroll-bottom" | "bottom" => Command::ScrollBottom,
            "hint" | "hints" => Command::Hint,
            "mode" | "set-mode" => Command::SetMode(arg()),
            "theme" | "theme-reload" => Command::Theme,
            "status" => Command::Status,
            "quit" | "q" => Command::Quit,
            other => return Err(format!("unknown command: {other}")),
        })
    }
}

/// The outcome of applying a command to the session, for the caller to enact
/// on the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Navigate the active tab (engine must load this URL).
    Navigate(String),
    /// Open a new tab and navigate it.
    NewTab(String),
    /// Reload the active tab.
    Reload,
    /// Scroll by this delta (in CSS pixels).
    Scroll { dx: f64, dy: f64 },
    /// Change input mode.
    SetMode(String),
    /// Turn link hints on.
    Hints,
    /// Re-read the theme from its source.
    ReloadTheme,
    /// Close the window.
    Quit,
    /// Nothing for the engine to do.
    None,
}

impl Session {
    /// Apply a parsed command, returning the effect the engine must perform.
    pub fn apply(&mut self, cmd: Command) -> Effect {
        match cmd {
            Command::Open(url) => {
                if url.trim().is_empty() {
                    return Effect::None;
                }
                self.current_mut().visit(url.clone());
                Effect::Navigate(url)
            }
            Command::OpenNewTab(url) => {
                if url.trim().is_empty() {
                    return Effect::None;
                }
                self.new_tab(url.clone());
                Effect::NewTab(url)
            }
            Command::Close => {
                self.close_active();
                Effect::None
            }
            Command::Select(i) => {
                self.select(i);
                Effect::None
            }
            Command::Next => {
                self.next_tab();
                Effect::None
            }
            Command::Prev => {
                self.prev_tab();
                Effect::None
            }
            Command::Back => match self.current_mut().back() {
                Some(url) => Effect::Navigate(url.to_string()),
                None => Effect::None,
            },
            Command::Forward => match self.current_mut().forward() {
                Some(url) => Effect::Navigate(url.to_string()),
                None => Effect::None,
            },
            Command::Reload => Effect::Reload,
            Command::Scroll { dx, dy } => Effect::Scroll { dx, dy },
            Command::ScrollTop => Effect::Scroll { dx: 0.0, dy: f64::MIN },
            Command::ScrollBottom => Effect::Scroll { dx: 0.0, dy: f64::MAX },
            Command::Hint => Effect::Hints,
            Command::SetMode(m) => Effect::SetMode(m),
            Command::Theme => Effect::ReloadTheme,
            Command::Quit => Effect::Quit,
            Command::Status => Effect::None,
        }
    }

    /// Handle a raw `serf-msg` line, producing the JSON reply and the effect.
    pub fn handle_ipc(&mut self, line: &str) -> (Response, Effect) {
        let cmd = match Command::parse(line) {
            Ok(c) => c,
            Err(e) => return (Response::err(e), Effect::None),
        };
        if cmd == Command::Status {
            return (self.status_response(), Effect::None);
        }
        let effect = self.apply(cmd);
        let response = Response::ok(match &effect {
            Effect::Navigate(u) => format!("opening {u}"),
            Effect::NewTab(u) => format!("new tab: {u}"),
            Effect::Reload => "reloading".into(),
            Effect::Scroll { dy, .. } => format!("scrolling by {dy}"),
            Effect::SetMode(m) => format!("mode {m}"),
            Effect::Hints => "hints".into(),
            Effect::ReloadTheme => "theme reloaded".into(),
            Effect::Quit => "quitting".into(),
            Effect::None => "ok".into(),
        });
        (response, effect)
    }

    fn status_response(&self) -> Response {
        let tabs: Vec<serde_json::Value> = self
            .tabs
            .iter()
            .map(|t| {
                serde_json::json!({
                    "title": t.title,
                    "url": t.current(),
                })
            })
            .collect();
        Response::ok(self.current().current().to_string()).with_data(serde_json::json!({
            "active": self.active,
            "tabs": tabs,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tab_history_and_navigation() {
        let mut t = Tab::new("https://a/");
        t.visit("https://b/");
        t.visit("https://c/");
        assert_eq!(t.current(), "https://c/");
        assert!(t.can_go_back());
        assert_eq!(t.back(), Some("https://b/"));
        assert_eq!(t.back(), Some("https://a/"));
        assert_eq!(t.back(), None);
        assert!(!t.can_go_back());
        assert_eq!(t.forward(), Some("https://b/"));
    }

    #[test]
    fn visiting_after_back_truncates_forward_history() {
        let mut t = Tab::new("https://a/");
        t.visit("https://b/");
        t.visit("https://c/");
        t.back();
        t.visit("https://d/");
        assert_eq!(t.current(), "https://d/");
        assert!(!t.can_go_forward());
        assert_eq!(t.history, vec!["https://a/", "https://b/", "https://d/"]);
    }

    #[test]
    fn revisiting_the_same_url_is_a_noop() {
        let mut t = Tab::new("https://a/");
        t.visit("https://a/");
        assert_eq!(t.history.len(), 1);
    }

    #[test]
    fn tab_cycling_wraps() {
        let mut s = Session::new("https://0/");
        s.new_tab("https://1/");
        s.new_tab("https://2/");
        assert_eq!(s.active(), 2);
        assert_eq!(s.next_tab(), 0);
        assert_eq!(s.prev_tab(), 2);
    }

    #[test]
    fn closing_the_last_tab_is_refused() {
        let mut s = Session::new("https://0/");
        assert_eq!(s.close_active(), None);
        assert_eq!(s.tabs().len(), 1);
    }

    #[test]
    fn closing_active_tab_keeps_an_index_in_range() {
        let mut s = Session::new("https://0/");
        s.new_tab("https://1/");
        s.new_tab("https://2/");
        // Close the last tab; active must fall back into range.
        assert_eq!(s.close_active(), Some(1));
        assert_eq!(s.active(), 1);
        assert_eq!(s.current().current(), "https://1/");
    }

    #[test]
    fn parses_the_common_commands() {
        assert_eq!(Command::parse("open https://x/").unwrap(), Command::Open("https://x/".into()));
        assert_eq!(Command::parse("tab-select 2").unwrap(), Command::Select(2));
        assert_eq!(Command::parse("scroll 120").unwrap(), Command::Scroll { dx: 0.0, dy: 120.0 });
        assert_eq!(Command::parse("scroll").unwrap(), Command::Scroll { dx: 0.0, dy: 80.0 });
        assert_eq!(Command::parse("mode insert").unwrap(), Command::SetMode("insert".into()));
        assert!(Command::parse("frobnicate").is_err());
        assert!(Command::parse("tab-select abc").is_err());
    }

    #[test]
    fn open_records_history_and_emits_navigate() {
        let mut s = Session::new("about:blank");
        let effect = s.apply(Command::Open("https://example.com/".into()));
        assert_eq!(effect, Effect::Navigate("https://example.com/".into()));
        assert_eq!(s.current().current(), "https://example.com/");
    }

    #[test]
    fn back_drives_the_engine_via_effect() {
        let mut s = Session::new("https://a/");
        s.apply(Command::Open("https://b/".into()));
        let effect = s.apply(Command::Back);
        assert_eq!(effect, Effect::Navigate("https://a/".into()));
    }

    #[test]
    fn ipc_status_reports_tabs() {
        let mut s = Session::new("https://a/");
        s.new_tab("https://b/");
        let (resp, effect) = s.handle_ipc("status");
        assert!(resp.ok);
        assert_eq!(effect, Effect::None);
        let data = resp.data.unwrap();
        assert_eq!(data["active"], 1);
        assert_eq!(data["tabs"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ipc_unknown_command_is_an_error_not_a_panic() {
        let mut s = Session::new("about:blank");
        let (resp, effect) = s.handle_ipc("nonsense");
        assert!(!resp.ok);
        assert_eq!(effect, Effect::None);
    }

    #[test]
    fn launcher_round_trip() {
        let mut s = Session::new("https://a/");
        s.new_tab("https://b/");
        s.tabs[0].title = "Alpha".into();
        s.tabs[1].title = "Beta".into();
        let listing = s.launcher_entries();
        assert_eq!(listing, "0: Alpha\n1: Beta");
        assert_eq!(s.parse_launcher_choice("1: Beta"), Some(1));
        assert_eq!(s.parse_launcher_choice("garbage"), None);
    }
}
