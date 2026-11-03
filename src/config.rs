//! Configuration loading, shared runtime state and live reload.
//!
//! The configuration is a plain TOML file (`~/.config/serf/config.toml`).
//! It is wrapped in an `Arc<RwLock<Config>>` so the input thread, the IPC
//! thread and the render loop can all read the current values, and so the
//! `notify` watcher can swap in a freshly parsed `Config` in place — no
//! restart, no recompilation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default socket path used for the `serf-msg` control channel.
pub const DEFAULT_SOCKET: &str = "/tmp/serf.sock";

/// Errors surfaced while locating or parsing the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not determine a config directory (no XDG_CONFIG_HOME, no $HOME)")]
    NoConfigDir,
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Top-level configuration document.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub adblock: AdblockConfig,
    pub theme: Theme,
    pub colors: Colors,
    pub overlay: Overlay,
    /// Per-domain overrides keyed by host.
    pub domains: BTreeMap<String, DomainRule>,
    pub keybindings: Keybindings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub homepage: String,
    pub socket_path: String,
    pub tab_launcher: String,
    pub search_engine: String,
    pub start_mode: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            homepage: "about:blank".into(),
            socket_path: DEFAULT_SOCKET.into(),
            tab_launcher: "fuzzel --dmenu".into(),
            search_engine: "https://duckduckgo.com/?q=%s".into(),
            start_mode: "normal".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdblockConfig {
    pub enabled: bool,
    pub lists: Vec<String>,
    pub cache: String,
    pub cosmetic: bool,
    pub tags: Vec<String>,
}

impl Default for AdblockConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lists: vec!["~/.local/share/serf/filters/easylist.txt".into()],
            cache: "~/.local/share/serf/filters/engine.dat".into(),
            cosmetic: true,
            tags: vec!["serf".into(), "linux".into()],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    /// `pywal`, `catppuccin` or `static`.
    pub source: String,
    pub catppuccin_flavour: String,
    pub inject_css: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            source: "catppuccin".into(),
            catppuccin_flavour: "mocha".into(),
            inject_css: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    pub background: String,
    pub foreground: String,
    pub accent: String,
    pub dim: String,
    pub alert: String,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: "#1e1e2e".into(),
            foreground: "#cdd6f4".into(),
            accent: "#89b4fa".into(),
            dim: "#6c7086".into(),
            alert: "#f38ba8".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overlay {
    pub height: u32,
    pub font_scale: u32,
    pub position: String,
    pub show_url_on_hover: bool,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            height: 22,
            font_scale: 1,
            position: "bottom".into(),
            show_url_on_hover: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DomainRule {
    pub zoom: Option<f32>,
    pub hide: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keybindings {
    pub normal: BTreeMap<String, String>,
    pub insert: BTreeMap<String, String>,
    pub command: BTreeMap<String, String>,
}

impl Config {
    /// Canonical config path: `$XDG_CONFIG_HOME/serf/config.toml`, falling
    /// back to `~/.config/serf/config.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok_or(ConfigError::NoConfigDir)?;
        Ok(base.join("serf").join("config.toml"))
    }

    /// Parse a config from a TOML string.
    pub fn from_str(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Load from a path; a missing file yields `Config::default()` so a fresh
    /// install still starts (surf-style: sane defaults, no wizard).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

/// Expand a leading `~` against `$HOME`.
pub fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if input == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(input)
}

/// A `Config` shared across threads, swappable at runtime.
pub type SharedConfig = Arc<RwLock<Config>>;

/// Wrap a config for shared, hot-reloadable access.
pub fn shared(cfg: Config) -> SharedConfig {
    Arc::new(RwLock::new(cfg))
}

/// Callback invoked after every successful reload.
pub type ReloadHook = Box<dyn Fn(&Config) + Send + 'static>;

/// Watch `path` and reload it into `store` on change.
///
/// Returns the watcher handle; dropping it stops watching. The watch is on
/// the *parent directory* because editors commonly replace the file by
/// rename, which breaks an inode watch.
///
/// Two guards keep this from feeding itself, which is not theoretical: the
/// inotify backend reports `Access(Open)`/`Access(Read)` as well as writes, so
/// a watcher that reacts to every event fires again on its *own* read of the
/// file the moment anything else reads it — a self-sustaining reload loop at
/// the rate of the settle sleep below. So: only content-changing event kinds
/// are considered, and a reload only happens when the text actually differs
/// from what was last applied.
pub fn spawn_watcher(
    store: SharedConfig,
    path: PathBuf,
    mut hook: Option<ReloadHook>,
) -> notify::Result<notify::RecommendedWatcher> {
    use notify::{Event, EventKind, RecursiveMode, Watcher};

    let watched_name = path
        .file_name()
        .map(|n| n.to_os_string());
    let reload_path = path.clone();
    // The text as of the last successful apply, so a same-content event
    // (`touch`, a redundant save, a duplicate inotify notification for one
    // write) is a no-op.
    let applied: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(event) = res else { return };

        // Reads report as `Access`; only actual content changes matter.
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }

        let touches_config = event
            .paths
            .iter()
            .any(|p| p.file_name() == watched_name.as_deref());
        if !touches_config {
            return;
        }

        // Give the writer a moment to finish the rename/write. This is a
        // settle delay on a real write, not a poll: no event, no wakeup.
        std::thread::sleep(Duration::from_millis(60));

        // A `Remove` (or a truncated file caught mid-write) just keeps the
        // previous config rather than resetting to defaults.
        let Ok(text) = std::fs::read_to_string(&reload_path) else {
            return;
        };
        match applied.lock() {
            Ok(current) if current.as_deref() == Some(text.as_str()) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        match Config::from_str(&text) {
            Ok(new_cfg) => {
                if let Ok(mut current) = applied.lock() {
                    *current = Some(text);
                }
                if let Ok(mut guard) = store.write() {
                    *guard = new_cfg.clone();
                }
                log::info!("serf: reloaded config from {}", reload_path.display());
                if let Some(hook) = hook.as_mut() {
                    hook(&new_cfg);
                }
            }
            Err(e) => log::warn!("serf: config reload failed, keeping previous: {e}"),
        }
    })?;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    watcher.watch(dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shipped_example_config() {
        let text = include_str!("../config.example.toml");
        let cfg = Config::from_str(text).expect("example config must parse");
        assert_eq!(cfg.general.homepage, "about:blank");
        assert!(cfg.adblock.enabled);
        assert_eq!(cfg.adblock.lists.len(), 1);
        assert_eq!(cfg.theme.source, "catppuccin");
        assert_eq!(
            cfg.keybindings.normal.get("j").map(String::as_str),
            Some("scroll-down")
        );
        // multi-chord binding survived
        assert_eq!(
            cfg.keybindings.normal.get("g g").map(String::as_str),
            Some("scroll-top")
        );
        // domain rule with extra cosmetic selectors
        let rule = cfg.domains.get("example.com").expect("domain rule");
        assert_eq!(rule.hide, vec!["#cookie-banner", ".newsletter-modal"]);
        assert_eq!(rule.zoom, Some(1.0));
    }

    #[test]
    fn rejects_unknown_keys_loudly() {
        let bad = "[general]\nhomepage = \"x\"\ntypo_key = 1\n";
        assert!(Config::from_str(bad).is_err());
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let cfg = Config::from_str("").unwrap();
        assert_eq!(cfg.general.socket_path, DEFAULT_SOCKET);
        assert_eq!(cfg.overlay.height, 22);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let cfg = Config::load(Path::new("/nonexistent/serf/config.toml")).unwrap();
        assert_eq!(cfg.general.start_mode, "normal");
    }

    #[test]
    fn tilde_expands() {
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(expand_tilde("~/x/y"), PathBuf::from("/home/tester/x/y"));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }

    /// Regression: the inotify backend reports `Access(Open/Read)` as well as
    /// writes, so a watcher that reacts to every event fires on its own read
    /// of the file — `serf-msg theme reload` (which re-reads the config) put
    /// the process into a self-sustaining ~15 reloads/second loop. Reads must
    /// be inert, and one content change must reload exactly once.
    #[test]
    fn reads_do_not_reload_but_a_write_reloads_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = std::env::temp_dir().join(format!("serf-watch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "[general]\nhomepage = \"about:blank\"\n").unwrap();

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let _watcher = spawn_watcher(
            shared(Config::default()),
            cfg_path.clone(),
            Some(Box::new(move |_cfg| {
                counter.fetch_add(1, Ordering::SeqCst);
            })),
        )
        .expect("watcher");

        // Let the watch settle before provoking anything.
        std::thread::sleep(Duration::from_millis(200));

        // What `Effect::ReloadTheme` does: re-read the file from disk.
        for _ in 0..50 {
            std::fs::read_to_string(&cfg_path).unwrap();
        }
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "reading the config must not trigger a reload"
        );

        // A real edit must still be picked up...
        std::fs::write(&cfg_path, "[general]\nhomepage = \"https://example.com/\"\n").unwrap();
        let mut waited = 0;
        while hits.load(Ordering::SeqCst) == 0 && waited < 3_000 {
            std::thread::sleep(Duration::from_millis(50));
            waited += 50;
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one write, one reload");

        // ...and the reload it performs must not itself cause another.
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the reload must not feed itself"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
