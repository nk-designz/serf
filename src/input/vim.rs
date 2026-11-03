//! The modal keybinding state machine.
//!
//! serf has three modes, mirroring vim:
//!
//! * [`Mode::Normal`] — keys are commands (`j`/`k` scroll, `f` hints, `o` open…).
//! * [`Mode::Insert`] — keys are forwarded to the page (except the escape key).
//! * [`Mode::Command`] — keys build an ex-style command line, submitted on Enter.
//!
//! Bindings are supplied by the config ([`crate::config::Keybindings`]) and
//! layered on top of [`Keymap::defaults`], so a user only lists what they want
//! to change. Multi-key chords (`g g`, `d d`) are supported: the machine keeps
//! a pending sequence and fires when it either completes a binding or can no
//! longer be a prefix of one.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::Keybindings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    Normal,
    Insert,
    Command,
}

impl Mode {
    /// The lowercase name shown in the status overlay.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Command => "command",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "normal" => Some(Mode::Normal),
            "insert" => Some(Mode::Insert),
            "command" | "cmd" => Some(Mode::Command),
            _ => None,
        }
    }
}

/// A single normalised key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Ctrl(char),
    /// `Alt`/`Meta` held with a character — `<A-x>`/`<M-x>` in a binding.
    Alt(char),
}

impl Key {
    /// Parse one token of a binding string, e.g. `j`, `G`, `<Escape>`, `<C-d>`.
    pub fn from_token(tok: &str) -> Option<Key> {
        if let Some(inner) = tok.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            return match inner.to_ascii_lowercase().as_str() {
                "escape" | "esc" => Some(Key::Escape),
                "enter" | "cr" => Some(Key::Enter),
                "backspace" | "bs" => Some(Key::Backspace),
                "tab" => Some(Key::Tab),
                "space" => Some(Key::Char(' ')),
                // `<C-x>` — a modifier prefix, not a three-letter word.
                other => {
                    // `<C-x>`, `<A-x>`/`<M-x>`. A modifier prefix, not a word.
                    let mut chars = other.chars();
                    match (chars.next(), chars.next(), chars.next()) {
                        (Some('c'), Some('-'), Some(c)) => Some(Key::Ctrl(c)),
                        (Some('a' | 'm'), Some('-'), Some(c)) => Some(Key::Alt(c)),
                        _ => None,
                    }
                }
            };
        }
        // Bare words that name a key. `<space>` was already accepted, but the
        // natural way to write a binding in a config is `space = "..."`, and
        // that used to fall through to the single-character path below, where
        // 's' followed by 'p' meant "not a key" and the whole binding was
        // dropped with "unparseable keybinding chord". Every alias here is
        // multi-character, so no single character binding is shadowed.
        match tok.to_ascii_lowercase().as_str() {
            "space" => return Some(Key::Char(' ')),
            "escape" | "esc" => return Some(Key::Escape),
            "enter" | "cr" | "return" => return Some(Key::Enter),
            "backspace" | "bs" => return Some(Key::Backspace),
            "tab" => return Some(Key::Tab),
            _ => {}
        }
        // `alt-x` / `meta-x` as a bare word, so a config can write
        // `alt-r = "reload"` without angle brackets (which a TOML bare key
        // cannot contain anyway).
        for prefix in ["alt-", "meta-", "a-", "m-"] {
            if let Some(rest) = tok.to_ascii_lowercase().strip_prefix(prefix) {
                let mut chars = rest.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    return Some(Key::Alt(c));
                }
            }
        }
        let mut chars = tok.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(Key::Char(c)),
            _ => None,
        }
    }

    /// The token used when echoing a pending chord in the status line.
    pub fn display(self) -> String {
        match self {
            Key::Char(' ') => "<Space>".into(),
            Key::Char(c) => c.to_string(),
            Key::Enter => "<Enter>".into(),
            Key::Escape => "<Esc>".into(),
            Key::Backspace => "<BS>".into(),
            Key::Tab => "<Tab>".into(),
            Key::Ctrl(c) => format!("<C-{c}>"),
            Key::Alt(c) => format!("<A-{c}>"),
        }
    }
}

/// A command the rest of the browser knows how to perform.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Scroll { dx: f64, dy: f64 },
    /// `direction`: -1 page up, +1 page down.
    HalfPage { direction: i8 },
    ScrollTop,
    ScrollBottom,
    Hint { new_tab: bool },
    OpenPrompt { edit: bool },
    Search { reverse: bool },
    HistoryBack,
    HistoryForward,
    Reload,
    TabNext,
    TabPrev,
    TabClose,
    TabNew,
    /// Open the external launcher (`tab_launcher`) on the tab list.
    TabMenu,
    YankUrl,
    PasteOpen,
    EnterMode(Mode),
    /// A submitted ex-command line (without the leading `:`).
    SubmitCommand(String),
    Quit,
    /// Key not bound here — forward it to the page (insert mode).
    PassThrough,
    /// Key consumed but nothing to do.
    None,
}

impl Action {
    /// Parse an action name from the config.
    /// The name a config binds this action with, or `None` for the variants
    /// that carry data (a submitted command line, a pass-through key).
    ///
    /// Kept next to [`Action::from_name`] so the two can be tested against each
    /// other: a binding documented in `config.example.toml` must survive the
    /// trip from name to action and back.
    pub fn name(&self) -> Option<&'static str> {
        Some(match self {
            Action::Scroll { dx: 0.0, dy } if *dy < 0.0 => "scroll-up",
            Action::Scroll { dx: 0.0, dy } if *dy > 0.0 => "scroll-down",
            Action::Scroll { dx, dy: 0.0 } if *dx < 0.0 => "scroll-left",
            Action::Scroll { dx, dy: 0.0 } if *dx > 0.0 => "scroll-right",
            Action::HalfPage { direction: -1 } => "half-page-up",
            Action::HalfPage { direction: 1 } => "half-page-down",
            Action::ScrollTop => "scroll-top",
            Action::ScrollBottom => "scroll-bottom",
            Action::Hint { new_tab: false } => "hint",
            Action::Hint { new_tab: true } => "hint-new-tab",
            Action::OpenPrompt { edit: false } => "open-prompt",
            Action::OpenPrompt { edit: true } => "open-prompt-edit",
            Action::Search { reverse: false } => "search",
            Action::Search { reverse: true } => "search-reverse",
            Action::HistoryBack => "history-back",
            Action::HistoryForward => "history-forward",
            Action::Reload => "reload",
            Action::TabNext => "tab-next",
            Action::TabPrev => "tab-prev",
            Action::TabClose => "tab-close",
            Action::TabNew => "tab-new",
            Action::TabMenu => "tab-menu",
            Action::YankUrl => "yank-url",
            Action::PasteOpen => "paste-open",
            Action::EnterMode(Mode::Normal) => "enter-normal",
            Action::EnterMode(Mode::Insert) => "enter-insert",
            Action::EnterMode(Mode::Command) => "enter-command",
            Action::Quit => "quit",
            Action::SubmitCommand(line) if line.is_empty() => "command-submit",
            _ => return None,
        })
    }

    pub fn from_name(name: &str) -> Option<Action> {
        Some(match name {
            "scroll-up" => Action::Scroll { dx: 0.0, dy: -80.0 },
            "scroll-down" => Action::Scroll { dx: 0.0, dy: 80.0 },
            "scroll-left" => Action::Scroll { dx: -80.0, dy: 0.0 },
            "scroll-right" => Action::Scroll { dx: 80.0, dy: 0.0 },
            "half-page-up" => Action::HalfPage { direction: -1 },
            "half-page-down" => Action::HalfPage { direction: 1 },
            "scroll-top" => Action::ScrollTop,
            "scroll-bottom" => Action::ScrollBottom,
            "hint" => Action::Hint { new_tab: false },
            "hint-new-tab" => Action::Hint { new_tab: true },
            "open-prompt" => Action::OpenPrompt { edit: false },
            "open-prompt-edit" => Action::OpenPrompt { edit: true },
            "search" => Action::Search { reverse: false },
            "search-reverse" => Action::Search { reverse: true },
            "history-back" => Action::HistoryBack,
            "history-forward" => Action::HistoryForward,
            "reload" => Action::Reload,
            "tab-next" => Action::TabNext,
            "tab-prev" => Action::TabPrev,
            "tab-close" => Action::TabClose,
            "tab-new" => Action::TabNew,
            "tab-menu" => Action::TabMenu,
            "yank-url" => Action::YankUrl,
            "paste-open" => Action::PasteOpen,
            "enter-normal" => Action::EnterMode(Mode::Normal),
            "enter-insert" => Action::EnterMode(Mode::Insert),
            "enter-command" => Action::EnterMode(Mode::Command),
            // Submit whatever is on the command line (the default for <Enter> in
            // command mode; named so a config can rebind it to another key).
            "command-submit" => Action::SubmitCommand(String::new()),
            "quit" => Action::Quit,
            _ => return None,
        })
    }
}

type Seq = Vec<Key>;

/// A resolved keymap for all three modes.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    normal: HashMap<Seq, Action>,
    insert: HashMap<Seq, Action>,
    command: HashMap<Seq, Action>,
}

fn parse_chord(chord: &str) -> Option<Seq> {
    let keys: Vec<Key> = chord
        .split_whitespace()
        .map(Key::from_token)
        .collect::<Option<_>>()?;
    (!keys.is_empty()).then_some(keys)
}

impl Keymap {
    /// The built-in vim-style bindings.
    pub fn defaults() -> Self {
        let mut normal: HashMap<Seq, Action> = HashMap::new();
        let builtin: &[(&str, &str)] = &[
            ("j", "scroll-down"),
            ("k", "scroll-up"),
            ("h", "scroll-left"),
            ("l", "scroll-right"),
            ("<C-d>", "half-page-down"),
            ("<C-u>", "half-page-up"),
            ("d", "half-page-down"),
            ("u", "half-page-up"),
            ("g g", "scroll-top"),
            ("G", "scroll-bottom"),
            ("f", "hint"),
            ("F", "hint-new-tab"),
            ("o", "open-prompt"),
            ("O", "open-prompt-edit"),
            ("H", "history-back"),
            ("L", "history-forward"),
            ("r", "reload"),
            ("J", "tab-prev"),
            ("K", "tab-next"),
            ("x", "tab-close"),
            ("t", "tab-new"),
            ("T", "tab-menu"),
            (":", "enter-command"),
            ("/", "search"),
            ("i", "enter-insert"),
            ("y", "yank-url"),
            ("p", "paste-open"),
            ("q", "quit"),
        ];
        for (chord, name) in builtin {
            if let (Some(seq), Some(action)) = (parse_chord(chord), Action::from_name(name)) {
                normal.insert(seq, action);
            }
        }

        let mut insert = HashMap::new();
        insert.insert(vec![Key::Escape], Action::EnterMode(Mode::Normal));

        let mut command = HashMap::new();
        command.insert(vec![Key::Escape], Action::EnterMode(Mode::Normal));
        command.insert(vec![Key::Enter], Action::SubmitCommand(String::new()));

        Self {
            normal,
            insert,
            command,
        }
    }

    /// Overlay the user's config on top of the defaults.
    /// Bindings in `kb` that will not take effect, as messages ready to log.
    ///
    /// A typo in an action name currently only produces a log line deep in the
    /// merge; collecting them up front lets the event loop report it once, at a
    /// level that shows under the default `RUST_LOG=warn`, instead of leaving
    /// the user with a key that silently does nothing.
    pub fn problems(kb: &Keybindings) -> Vec<String> {
        let mut problems = Vec::new();
        let modes: [(&str, &std::collections::BTreeMap<String, String>); 3] = [
            ("normal", &kb.normal),
            ("insert", &kb.insert),
            ("command", &kb.command),
        ];
        for (mode, table) in modes {
            for (chord, name) in table {
                if parse_chord(chord).is_none() {
                    problems.push(format!("{mode}: cannot read chord {chord:?}"));
                } else if !name.trim().eq_ignore_ascii_case("none")
                    && Action::from_name(name).is_none()
                {
                    problems.push(format!("{mode}: unknown action {name:?} for {chord:?}"));
                }
            }
        }
        problems
    }

    pub fn with_overrides(mut self, kb: &Keybindings) -> Self {
        apply_table(&mut self.normal, &kb.normal);
        apply_table(&mut self.insert, &kb.insert);
        apply_table(&mut self.command, &kb.command);
        self
    }

    fn table(&self, mode: Mode) -> &HashMap<Seq, Action> {
        match mode {
            Mode::Normal => &self.normal,
            Mode::Insert => &self.insert,
            Mode::Command => &self.command,
        }
    }
}

fn apply_table(table: &mut HashMap<Seq, Action>, src: &std::collections::BTreeMap<String, String>) {
    for (chord, name) in src {
        // `none` is how a config *removes* a vim default instead of adding to
        // it: the defaults are always merged first, so without this there is no
        // way to free up a key the user does not want.
        if name.trim().eq_ignore_ascii_case("none") {
            match parse_chord(chord) {
                Some(seq) => {
                    table.remove(&seq);
                }
                None => log::warn!("serf: unparseable keybinding chord {chord:?}"),
            }
            continue;
        }
        match (parse_chord(chord), Action::from_name(name)) {
            (Some(seq), Some(action)) => {
                table.insert(seq, action);
            }
            (None, _) => log::warn!("serf: unparseable keybinding chord {chord:?}"),
            (_, None) => log::warn!("serf: unknown action {name:?} for chord {chord:?}"),
        }
    }
}

/// The live state of the modal input machine.
pub struct VimState {
    pub mode: Mode,
    keymap: Keymap,
    pending: Seq,
    command: String,
    /// Number prefix typed before a normal-mode command (e.g. `5j`).
    count: Option<u32>,
}

impl VimState {
    pub fn new(keymap: Keymap, start: Mode) -> Self {
        Self {
            mode: start,
            keymap,
            pending: Seq::new(),
            command: String::new(),
            count: None,
        }
    }

    /// The text typed on the `:` line so far (for the overlay).
    pub fn command_line(&self) -> &str {
        &self.command
    }

    /// Prefill the open prompt with the active page URL (`O`).
    pub fn prefill_open_url(&mut self, url: &str) {
        debug_assert_eq!(self.mode, Mode::Command);
        self.command = format!("open {url}");
    }

    /// The pending chord, rendered for the status overlay (empty if none).
    pub fn pending_display(&self) -> String {
        self.pending
            .iter()
            .map(|k| k.display())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Replace the keymap (called on config hot-reload).
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.pending.clear();
    }

    /// Enter a mode without a keybinding (e.g. pointer click into page content).
    pub fn enter_mode(&mut self, mode: Mode) {
        self.apply(Action::EnterMode(mode));
    }

    /// Feed one key and get the resulting action, if any.
    pub fn feed(&mut self, key: Key) -> Action {
        match self.mode {
            Mode::Command => self.feed_command(key),
            Mode::Insert => {
                match self.keymap.table(Mode::Insert).get(&vec![key]) {
                    Some(action) => self.apply(action.clone()),
                    None => Action::PassThrough,
                }
            }
            Mode::Normal => self.feed_normal(key),
        }
    }

    fn feed_command(&mut self, key: Key) -> Action {
        if let Some(action) = self.keymap.table(Mode::Command).get(&vec![key]) {
            let action = action.clone();
            // `<Enter>` is bound to a placeholder — fill in the real text.
            let action = match action {
                Action::SubmitCommand(_) => {
                    let line = std::mem::take(&mut self.command);
                    Action::SubmitCommand(line)
                }
                other => other,
            };
            return self.apply(action);
        }
        match key {
            Key::Char(c) => {
                self.command.push(c);
                Action::None
            }
            Key::Backspace => {
                self.command.pop();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn feed_normal(&mut self, key: Key) -> Action {
        // Numeric count prefix (1-9, then 0-9).
        if let Key::Char(c) = key {
            if c.is_ascii_digit() && (self.count.is_some() || c != '0') {
                self.count = Some(self.count.unwrap_or(0) * 10 + c.to_digit(10).unwrap());
                return Action::None;
            }
        }

        let mut seq = self.pending.clone();
        seq.push(key);
        let table = self.keymap.table(Mode::Normal);

        if let Some(action) = table.get(&seq) {
            self.pending.clear();
            return self.apply(action.clone());
        }

        // Is `seq` still a prefix of some binding?
        let is_prefix = table.keys().any(|k| k.len() > seq.len() && k.starts_with(&seq));
        if is_prefix {
            self.pending = seq;
            return Action::None;
        }

        // Dead end: drop the pending chord and try the key on its own.
        if !self.pending.is_empty() {
            self.pending.clear();
            return self.feed_normal(key);
        }
        self.count = None;
        Action::None
    }

    fn apply(&mut self, action: Action) -> Action {
        match &action {
            Action::EnterMode(mode) => {
                self.mode = *mode;
                self.pending.clear();
                if *mode != Mode::Command {
                    self.command.clear();
                }
            }
            Action::OpenPrompt { .. } => {
                self.mode = Mode::Command;
                self.pending.clear();
                self.command.clear();
                self.command.push_str("open ");
            }
            Action::SubmitCommand(_) => {
                self.mode = Mode::Normal;
                self.pending.clear();
            }
            _ => {}
        }
        // Attach a numeric count to scrolling actions.
        let count = self.count.take().unwrap_or(1) as f64;
        match action {
            Action::Scroll { dx, dy } => Action::Scroll {
                dx: dx * count,
                dy: dy * count,
            },
            Action::HalfPage { direction } => Action::HalfPage { direction },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> VimState {
        VimState::new(Keymap::defaults(), Mode::Normal)
    }

    fn ch(c: char) -> Key {
        Key::Char(c)
    }

    #[test]
    fn j_scrolls_down() {
        let mut s = state();
        assert_eq!(s.feed(ch('j')), Action::Scroll { dx: 0.0, dy: 80.0 });
    }

    #[test]
    fn count_prefix_multiplies_scroll() {
        let mut s = state();
        assert_eq!(s.feed(ch('3')), Action::None);
        assert_eq!(s.feed(ch('j')), Action::Scroll { dx: 0.0, dy: 240.0 });
    }

    #[test]
    fn multi_key_chord_g_g_jumps_to_top() {
        let mut s = state();
        assert_eq!(s.feed(ch('g')), Action::None); // pending
        assert_eq!(s.pending_display(), "g");
        assert_eq!(s.feed(ch('g')), Action::ScrollTop);
        assert_eq!(s.pending_display(), "");
    }

    #[test]
    fn dead_end_chord_recovers_to_single_key() {
        let mut s = state();
        // `g` then `z`: `g z` is not bound, but `z` alone is not either -> None,
        // and the pending `g` must be cleared.
        assert_eq!(s.feed(ch('g')), Action::None);
        assert_eq!(s.feed(ch('z')), Action::None);
        assert_eq!(s.pending_display(), "");
    }

    #[test]
    fn f_requests_hints() {
        let mut s = state();
        assert_eq!(s.feed(ch('f')), Action::Hint { new_tab: false });
        assert_eq!(s.feed(ch('F')), Action::Hint { new_tab: true });
    }

    #[test]
    fn enter_and_leave_insert_mode() {
        let mut s = state();
        assert_eq!(s.feed(ch('i')), Action::EnterMode(Mode::Insert));
        assert_eq!(s.mode, Mode::Insert);
        // An unbound key in insert mode is passed to the page.
        assert_eq!(s.feed(ch('x')), Action::PassThrough);
        assert_eq!(s.feed(Key::Escape), Action::EnterMode(Mode::Normal));
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn page_click_switches_to_insert_so_a_clicked_field_can_type() {
        let mut s = state();
        s.enter_mode(Mode::Insert);
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(s.feed(ch('t')), Action::PassThrough);
        assert_eq!(s.feed(Key::Escape), Action::EnterMode(Mode::Normal));
    }

    #[test]
    fn command_line_accumulates_and_submits() {
        let mut s = state();
        assert_eq!(s.feed(ch(':')), Action::EnterMode(Mode::Command));
        assert_eq!(s.mode, Mode::Command);
        s.feed(ch('o'));
        s.feed(ch('p'));
        s.feed(ch('e'));
        s.feed(ch('n'));
        assert_eq!(s.command_line(), "open");
        assert_eq!(
            s.feed(Key::Enter),
            Action::SubmitCommand("open".to_string())
        );
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.command_line(), "");
    }

    #[test]
    fn open_prompt_accepts_a_url_and_submits_open_command() {
        let mut s = state();
        assert_eq!(s.feed(ch('o')), Action::OpenPrompt { edit: false });
        assert_eq!(s.mode, Mode::Command);
        assert_eq!(s.command_line(), "open ");
        for c in "https://duckduckgo.com/".chars() {
            s.feed(ch(c));
        }
        assert_eq!(
            s.feed(Key::Enter),
            Action::SubmitCommand("open https://duckduckgo.com/".into())
        );
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn edit_open_prompt_prefills_current_url() {
        let mut s = state();
        assert_eq!(s.feed(ch('O')), Action::OpenPrompt { edit: true });
        assert_eq!(s.mode, Mode::Command);
        s.prefill_open_url("https://example.com/");
        assert_eq!(s.command_line(), "open https://example.com/");
        assert_eq!(
            s.feed(Key::Enter),
            Action::SubmitCommand("open https://example.com/".into())
        );
    }

    #[test]
    fn command_backspace_edits() {
        let mut s = state();
        s.feed(ch(':'));
        s.feed(ch('q'));
        s.feed(ch('t'));
        s.feed(Key::Backspace);
        assert_eq!(s.command_line(), "q");
    }

    #[test]
    fn config_overrides_win_over_defaults() {
        let mut kb = Keybindings::default();
        kb.normal.insert("j".into(), "scroll-up".into()); // swap direction
        // A *new* multi-key chord. It must start with a key that is unbound on
        // its own, otherwise the first press would resolve immediately.
        kb.normal.insert("z z".into(), "quit".into());
        let keymap = Keymap::defaults().with_overrides(&kb);
        let mut s = VimState::new(keymap, Mode::Normal);
        assert_eq!(s.feed(ch('j')), Action::Scroll { dx: 0.0, dy: -80.0 });
        assert_eq!(s.feed(ch('z')), Action::None); // pending — waiting for `z z`
        assert_eq!(s.feed(ch('z')), Action::Quit);
    }

    #[test]
    fn key_token_parsing() {
        assert_eq!(Key::from_token("g"), Some(Key::Char('g')));
        assert_eq!(Key::from_token("<Escape>"), Some(Key::Escape));
        assert_eq!(Key::from_token("<C-d>"), Some(Key::Ctrl('d')));
        assert_eq!(Key::from_token("<Space>"), Some(Key::Char(' ')));
        assert_eq!(Key::from_token("gg"), None);
    }

    #[test]
    fn bare_key_names_parse_too() {
        // Configs are written `space = "half-page-down"`, without angle brackets,
        // and a bare "space" used to be rejected as an unparseable chord.
        assert_eq!(Key::from_token("space"), Some(Key::Char(' ')));
        assert_eq!(Key::from_token("esc"), Some(Key::Escape));
        assert_eq!(Key::from_token("enter"), Some(Key::Enter));
        assert_eq!(Key::from_token("tab"), Some(Key::Tab));
        // A single character must still win over the word fallback.
        assert_eq!(Key::from_token("j"), Some(Key::Char('j')));
    }

    #[test]
    fn mode_names_round_trip() {
        for m in [Mode::Normal, Mode::Insert, Mode::Command] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
    }

    #[test]
    fn every_named_action_round_trips_through_its_name() {
        // The list a config author sees. If a variant gains a name in
        // `from_name` and not in `name`, `:map`-style listings and this test
        // disagree, which is exactly the drift worth catching.
        for name in [
            "scroll-up", "scroll-down", "scroll-left", "scroll-right",
            "half-page-up", "half-page-down", "scroll-top", "scroll-bottom",
            "hint", "hint-new-tab", "open-prompt", "open-prompt-edit",
            "search", "search-reverse", "history-back", "history-forward",
            "reload", "tab-next", "tab-prev", "tab-close", "tab-new",
            "tab-menu", "yank-url", "paste-open", "enter-normal",
            "enter-insert", "enter-command", "quit", "command-submit",
        ] {
            let action = Action::from_name(name).unwrap_or_else(|| panic!("{name} is not an action"));
            assert_eq!(action.name(), Some(name), "{name} does not round-trip");
        }
    }

    #[test]
    fn every_vim_default_binds_an_action_that_has_a_name() {
        // A default whose action cannot be named cannot be overridden back to
        // that action from a config, which would make the defaults one-way.
        for (seq, action) in Keymap::defaults().normal {
            assert!(action.name().is_some(), "{seq:?} binds {action:?}, which has no name");
        }
    }

    #[test]
    fn a_mistyped_binding_is_reported_instead_of_ignored() {
        let mut kb = Keybindings::default();
        kb.normal.insert("j".into(), "scroll-dwn".into());
        kb.normal.insert("nonsense".into(), "reload".into());
        kb.normal.insert("q".into(), "none".into());
        let problems = Keymap::problems(&kb);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("scroll-dwn")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("nonsense")), "{problems:?}");
        // `none` is a valid instruction, not a mistake.
        assert!(!problems.iter().any(|p| p.contains("none")), "{problems:?}");
    }

    #[test]
    fn none_unbinds_a_vim_default() {
        let mut kb = Keybindings::default();
        kb.normal.insert("q".into(), "none".into());
        let map = Keymap::defaults().with_overrides(&kb);
        assert_eq!(map.normal.get(&vec![Key::Char('q')]), None);
        // Everything else survives: removing one key is not clearing the table.
        assert_eq!(map.normal.get(&vec![Key::Char('j')]), Some(&Action::Scroll { dx: 0.0, dy: 80.0 }));
    }

    #[test]
    fn alt_chords_bind_like_vim_control_chords() {
        assert_eq!(Key::from_token("<A-r>"), Some(Key::Alt('r')));
        assert_eq!(Key::from_token("<M-r>"), Some(Key::Alt('r')));
        // The bare form exists because TOML bare keys cannot contain `<>`.
        assert_eq!(Key::from_token("alt-r"), Some(Key::Alt('r')));
        let mut kb = Keybindings::default();
        kb.normal.insert("alt-r".into(), "reload".into());
        let map = Keymap::defaults().with_overrides(&kb);
        assert_eq!(map.normal.get(&vec![Key::Alt('r')]), Some(&Action::Reload));
    }
}
