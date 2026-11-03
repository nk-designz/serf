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
                    let mut chars = other.chars();
                    match (chars.next(), chars.next(), chars.next()) {
                        (Some('c'), Some('-'), Some(c)) => Some(Key::Ctrl(c)),
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
}
