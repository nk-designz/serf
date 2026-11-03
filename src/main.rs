//! serf — a suckless, keyboard-driven Servo browser for Wayland.
//!
//! Startup order:
//!   1. parse arguments, load + watch the config;
//!   2. compile/ mmap the adblock engine;
//!   3. bind the `serf-msg` socket and start its thread;
//!   4. create the Wayland window and the engine backend;
//!   5. spawn the `wlr-layer-shell` status overlay on its own thread;
//!   6. run the winit event loop, translating keys through the modal input
//!      machine and the tab/history model.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Window, WindowId};

use serf::adblock::AdblockEngine;
use serf::browser::engine::{
    ControlChoice, ControlRequest, Engine, LinkSink, PageKey, PageState, StubEngine,
};
use serf::browser::launcher::Launcher;
use serf::browser::navigation::{Command, Effect, Session};
use serf::config::{self, Config, SharedConfig};
use serf::input::gesture::{Swipe, SwipeAccumulator};
use serf::input::hints::{HintOutcome, HintSession, LinkBox};
use serf::input::vim::{Action, Key, Keymap, Mode, VimState};
use serf::ipc::{Response, Server};
use serf::ui::overlay::{self, OverlayState, StatusLine};
use serf::ui::theme::Theme;

/// `mimalloc` is the global allocator: fewer syscalls and much lower
/// fragmentation than glibc malloc on this workload (lots of small, short
/// lived DOM/network allocations), which matters on a 4 GB ARM box.
///
/// Stub build only. With `--features servo` the engine links `servo_allocator`,
/// which installs a `#[global_allocator]` of its own, and two of those in one
/// binary is a hard error (E0152). Servo's allocator wins there: it is tuned
/// for the engine's own allocation pattern, and overriding it from outside
/// would be the wrong call even if it were legal.
#[cfg(not(feature = "servo"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Events delivered to the winit loop from other threads.
#[derive(Debug)]
enum UserEvent {
    /// An effect produced by a `serf-msg` command.
    Effect(Box<Effect>),
    /// Servo has work for the loop — a script message, or a frame that is ready to
    /// paint. It carries nothing: the work lives in Servo, and `App::user_event`
    /// spins it. This is the variant `ServoWaker` sends, and it exists because a
    /// wake that does not reach winit is a wake that never happened.
    ///
    /// Only the servo build has a waker to send it, so only the servo build has it.
    #[cfg(feature = "servo")]
    Wake,
}

/// State shared between the main thread and the IPC/overlay threads.
///
/// NB: the adblock engine is deliberately *not* here. `adblock::Engine`
/// holds an `Rc` internally and is therefore not `Send`, so it cannot be
/// reached from the IPC thread — it lives on the main thread in `App` and is
/// only consulted there when a document is (re)styled.
struct Shared {
    config: SharedConfig,
    session: Mutex<Session>,
    overlay: Arc<OverlayState>,
    /// Requests the filter engine cancelled, so `serf-msg status`/`stats` can
    /// report something a human can check without reading logs.
    blocked: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Requests the page layer asked the filter about, blocked or not.
    filtered: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Why the filter engine is not loaded, if it is not. A silent `None` here
    /// means "no network adblocking" and nothing else says so.
    adblock_error: Option<String>,
}

fn main() -> Result<()> {
    // Exactly one thing may install the process-wide logger: `log::set_logger`
    // succeeds once per process and every later call fails. In the servo build
    // `servo.setup_logging()` (from `create_engine`) is that owner — it installs
    // its own env_logger *and* a logger that routes engine logs to the
    // constellation channel. Installing ours here as well made that second call
    // panic with `Failed to set logger.: SetLoggerError(())`, which is fatal and
    // killed the binary before it could paint its first frame. So the default
    // build owns logging, and the servo build defers to Servo; set `RUST_LOG`
    // to control the servo build's level.
    #[cfg(not(feature = "servo"))]
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp(None)
        .init();

    let mut config_path: Option<PathBuf> = None;
    let mut homepage: Option<String> = None;
    let mut build_cache = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            "-V" | "--version" => {
                println!("serf {} ({})", env!("CARGO_PKG_VERSION"), backend_name());
                return Ok(());
            }
            "-c" | "--config" => {
                config_path = Some(PathBuf::from(
                    args.next().context("--config needs a path")?,
                ));
            }
            "--build-adblock-cache" => build_cache = true,
            other if other.starts_with('-') => {
                anyhow::bail!("unknown option {other:?} (try --help)");
            }
            url => homepage = Some(url.to_string()),
        }
    }

    let (config_path, named) = match config_path {
        Some(p) => (p, true),
        None => (Config::default_path()?, false),
    };
    // **A named config that is not there is a typo, not a fresh install.** A missing
    // file is tolerated only at the *default* path, where "no config yet, run on the
    // defaults" is the surf-style greeting. Reaching this with `-c /tmp/serf-probe.toml`
    // whose name was mistyped used to start the whole session on `~/.config/serf/config.toml`
    // — which is a measurement against a config nobody chose, and it looks like a
    // success. Same spirit as `deny_unknown_fields`: fail loudly at startup.
    if named && !config_path.exists() {
        anyhow::bail!(
            "no such config file: {} (omit -c/--config to run on the defaults)",
            config_path.display()
        );
    }
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;

    // --build-adblock-cache: compile the lists, write the mmap'd engine, exit.
    if build_cache {
        return build_adblock_cache(&cfg);
    }

    let startup_url = homepage.unwrap_or_else(|| cfg.general.homepage.clone());
    let theme = Theme::resolve(&cfg.theme, &cfg.colors);

    // Compile or mmap the adblock engine.
    let mut adblock_error = None;
    let adblock = if cfg.adblock.enabled {
        let lists: Vec<PathBuf> = cfg.adblock.lists.iter().map(|s| config::expand_tilde(s)).collect();
        let cache = config::expand_tilde(&cfg.adblock.cache);
        match AdblockEngine::load_or_build(&lists, &cache, &cfg.adblock.tags) {
            Ok(engine) => Some(std::rc::Rc::new(engine)),
            Err(e) => {
                log::warn!("serf: adblock disabled ({e}); continuing without it");
                adblock_error = Some(format!("{e:#}"));
                None
            }
        }
    } else {
        None
    };

    let store = config::shared(cfg.clone());
    let session = Session::new(startup_url.clone());

    let overlay = Arc::new(OverlayState::new(
        StatusLine {
            mode: "normal".into(),
            title: startup_url.clone(),
            url: startup_url.clone(),
            ..Default::default()
        },
        theme.palette,
        false,
    ));

    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let filtered = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let shared = Arc::new(Shared {
        config: store.clone(),
        session: Mutex::new(session),
        overlay: overlay.clone(),
        blocked: blocked.clone(),
        filtered: filtered.clone(),
        adblock_error,
    });

    // Start the layer-shell status overlay on its own connection/thread. A
    // compositor without wlr-layer-shell just logs and carries on.
    let _overlay_thread = overlay::spawn_layer_overlay(
        overlay.clone(),
        cfg.overlay.height,
        cfg.overlay.position.clone(),
        cfg.overlay.font_scale,
    );

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("creating the winit event loop (is WAYLAND_DISPLAY set?)")?;
    let proxy = event_loop.create_proxy();

    // Bind the control socket and serve it from its own thread.
    let socket_path = PathBuf::from(&cfg.general.socket_path);
    let server = match Server::bind(&socket_path) {
        Ok(server) => {
            log::info!("serf-msg listening on {}", socket_path.display());
            let shared = shared.clone();
            let proxy = proxy.clone();
            Some(server.serve(move |line| serve_command(&shared, &proxy, line)))
        }
        Err(e) => {
            log::warn!("serf: cannot bind {} ({e}); serf-msg disabled", socket_path.display());
            None
        }
    };

    // Hot-reload the config when it changes on disk.
    let _watcher = config::spawn_watcher(store.clone(), config_path.clone(), None).ok();

    let keymap = Keymap::defaults().with_overrides(&cfg.keybindings);
    let start_mode = Mode::parse(&cfg.general.start_mode).unwrap_or(Mode::Normal);

    let mut app = App {
        shared,
        vim: VimState::new(keymap, start_mode),
        hints: None,
        engine: None,
        window: None,
        theme,
        adblock,
        config_path,
        modifiers: ModifiersState::empty(),
        pointer: (0.0, 0.0),
        swipe: SwipeAccumulator::new(60.0),
        launcher: None,
        menu_purpose: None,
        wake: proxy.clone(),
    };

    event_loop.run_app(&mut app).context("running the winit event loop")?;
    drop(server);
    let _ = _watcher;
    let _ = _overlay_thread;
    Ok(())
}

fn serve_command(shared: &Arc<Shared>, proxy: &EventLoopProxy<UserEvent>, line: &str) -> Response {
    // `stats` is answered here rather than by the tab model: it reports what the
    // page layer did, which the session cannot see.
    if line.trim() == "stats" {
        let blocked = shared.blocked.load(std::sync::atomic::Ordering::Relaxed);
        let filtered = shared.filtered.load(std::sync::atomic::Ordering::Relaxed);
        return Response::ok(format!("blocked {blocked} of {filtered} filtered requests"))
            .with_data(serde_json::json!({
                "blocked": blocked,
                "filtered": filtered,
                "adblock_error": shared.adblock_error,
            }));
    }
    let (response, effect) = {
        let mut session = match shared.session.lock() {
            Ok(s) => s,
            Err(_) => return Response::err("session lock poisoned"),
        };
        session.handle_ipc(line)
    };
    push_status(shared);
    if let Err(e) = proxy.send_event(UserEvent::Effect(Box::new(effect))) {
        log::warn!("serf: cannot wake the event loop: {e}");
    }
    response
}

/// Recompute the overlay status line from the shared session.
fn push_status(shared: &Shared) {
    if let Ok(session) = shared.session.lock() {
        let tab = session.current();
        shared.overlay.set_status(StatusLine {
            mode: "normal".into(),
            title: tab.title.clone(),
            url: tab.current().to_string(),
            ..Default::default()
        });
    }
}

fn backend_name() -> &'static str {
    if cfg!(feature = "servo") {
        "servo"
    } else {
        "stub (build with --features servo for the real engine)"
    }
}

fn print_usage() {
    println!(
        "serf {} — suckless servo browser for wayland\n\
         \n\
         USAGE: serf [OPTIONS] [URL]\n\
         \n\
         OPTIONS:\n\
         \x20 -c, --config <PATH>        config file, must exist (default ~/.config/serf/config.toml)\n\
         \x20     --build-adblock-cache  compile the filter lists and exit\n\
         \x20 -V, --version              print the version and the engine backend\n\
         \x20 -h, --help                 this message\n\
         \n\
         KEYS: j/k scroll, f hints, o open, i insert, : command, H/L history,\n\
         \x20     J/K tabs, q quit — see config.example.toml for the full table.\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn build_adblock_cache(cfg: &Config) -> Result<()> {
    let lists: Vec<PathBuf> = cfg.adblock.lists.iter().map(|s| config::expand_tilde(s)).collect();
    for list in &lists {
        if !list.exists() {
            anyhow::bail!("filter list {} does not exist", list.display());
        }
    }
    let cache = config::expand_tilde(&cfg.adblock.cache);
    let engine = AdblockEngine::from_lists(&lists, &cfg.adblock.tags)
        .context("compiling the filter lists")?;
    engine.save_cache(&cache)?;
    println!("wrote adblock cache to {}", cache.display());
    Ok(())
}

/// The winit application.
struct App {
    shared: Arc<Shared>,
    vim: VimState,
    hints: Option<HintSession>,
    engine: Option<Box<dyn Engine>>,
    window: Option<Window>,
    theme: Theme,
    /// Main-thread-only: `adblock::Engine` is not `Send`. Shared by `Rc` with the
    /// page delegate, which runs the same network hook from the embedder thread.
    adblock: Option<Rc<AdblockEngine>>,
    config_path: PathBuf,
    /// Live modifier state. serf's keymap needs it to resolve `<C-d>`-style
    /// chords, and the page needs it on every forwarded key.
    modifiers: ModifiersState,
    /// Where the pointer was last seen, in device pixels — the origin a click or
    /// a scroll is resolved against.
    pointer: (f64, f64),
    /// Two-finger horizontal swipe state (`Ctrl` = history, `Ctrl+Shift` = tabs).
    swipe: SwipeAccumulator,
    /// A running external menu, if one is open.
    launcher: Option<Launcher>,
    /// What that menu is choosing, so its answer can be applied.
    menu_purpose: Option<MenuPurpose>,
    /// The winit wake handle, held so the Servo backend can be handed a waker that
    /// actually wakes this loop (see `ServoWaker`). Servo calls `wake` from its own
    /// threads, so this has to be the `Send + Sync` proxy and not the loop itself.
    wake: EventLoopProxy<UserEvent>,
}

/// What a running external menu is for.
#[derive(Debug, Clone, PartialEq)]
enum MenuPurpose {
    /// A tab, chosen by index.
    Tab,
    /// A form control the page is blocked on.
    Control(ControlRequest),
}

/// How much a wheel "line" is worth in device pixels. winit reports a discrete
/// wheel as lines and a trackpad as pixels; Servo takes pixels, so the discrete
/// case has to be scaled. 50 is the browser-typical step and matches what the
/// `j`/`k` scroll distance already feels like.
const PIXELS_PER_WHEEL_LINE: f64 = 50.0;

/// Whether a wheel delta is a horizontal swipe rather than a mostly-vertical
/// scroll. A touchpad never moves on exactly one axis, so "horizontal" means
/// the x component dominates; a pure wheel has no x component at all.
fn x_dominates(dx: f64, dy: f64) -> bool {
    dx != 0.0 && dx.abs() > dy.abs()
}

impl App {
    fn sync_status(&self) {
        let session = self.shared.session.lock().unwrap();
        let tab = session.current();
        let status = StatusLine {
            mode: self.vim.mode.as_str().to_string(),
            title: tab.title.clone(),
            url: tab.current().to_string(),
            pending: self.vim.pending_display(),
            message: self.vim.command_line().to_string(),
        };
        self.shared.overlay.set_visible(status.requires_overlay());
        self.shared.overlay.set_status(status);
    }

    // --- the page's side of the loop -------------------------------------

    /// Bring the model, the overlay and the window in line with everything the
    /// engine reported since the last turn.
    ///
    /// The delegate runs on Servo's threads and only records; this is where the
    /// records become actions. Everything is collected before anything is acted
    /// on, because acting borrows `self` mutably — the engine borrow has to end
    /// first.
    fn poll_engine(&mut self) {
        let (control, new_tabs, state, repaint, key_handled) = {
            let Some(engine) = self.engine.as_mut() else {
                return;
            };
            (
                engine.pending_control(),
                engine.take_new_tabs(),
                engine.take_page_state(),
                engine.wants_repaint(),
                engine.last_key_handled(),
            )
        };

        if repaint {
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
        }

        for url in new_tabs {
            log::info!("serf: the page opened a new tab: {url}");
            let effect = self
                .shared
                .session
                .lock()
                .unwrap()
                .apply(Command::OpenNewTab(url));
            self.apply_effect(effect);
        }

        if let Some(state) = state {
            self.apply_page_state(&state);
        }

        // A control blocks the page until it is answered, so open the menu for
        // it — unless one is already open.
        if let Some(request) = control {
            if self.menu_purpose.is_none() {
                let entries = control_entries(&request);
                self.start_menu(MenuPurpose::Control(request), entries);
            }
        }

        if let Some(handled) = key_handled {
            log::debug!(
                "serf: the page {} the last key",
                if handled { "took" } else { "ignored" }
            );
        }
    }

    /// Fold what the page reported into the tab model.
    fn apply_page_state(&mut self, state: &PageState) {
        for line in &state.console {
            log::info!("serf: console: {line}");
        }

        if let Some(reason) = state.crashed.as_ref() {
            log::error!("serf: the page crashed: {reason}");
            // The viewport is dead, so the bar is the only place left to say so.
            self.shared.session.lock().unwrap().current_mut().title = "⚠ crashed".into();
        }

        if let Some(fullscreen) = state.fullscreen {
            // A page asking for fullscreen is honoured: a video that cannot go
            // fullscreen is the one thing that would make this browser useless
            // for media.
            if let Some(window) = self.window.as_ref() {
                window.set_fullscreen(fullscreen.then_some(Fullscreen::Borderless(None)));
            }
        }

        if state.url.is_some() || state.title.is_some() {
            let mut session = self.shared.session.lock().unwrap();
            let tab = session.current_mut();
            // The engine is the authority on what is on screen, and its own
            // back/forward list is the real history. serf's per-tab history is a
            // convenience for the launcher, so a page-driven URL change updates
            // the current entry rather than pushing a second one for a
            // navigation serf already recorded.
            if let Some(url) = state.url.as_ref() {
                tab.replace(url.clone());
            }
            if let Some(title) = state.title.as_ref() {
                tab.title = title.clone();
            }
        }

        if let Some(loading) = state.loading {
            log::debug!("serf: {}", if loading { "loading" } else { "load complete" });
        }

        if state.url.is_some() || state.title.is_some() || state.loading.is_some() {
            self.sync_status();
        }
    }

    /// Open the external launcher on the tab list (`T`, or the `tab-menu` action).
    fn open_tab_menu(&mut self) {
        let entries = self.shared.session.lock().unwrap().launcher_entries();
        self.start_menu(MenuPurpose::Tab, entries);
    }

    /// Run the configured `tab_launcher` over `entries`.
    ///
    /// The menu owns its own window (fuzzel, wofi, dmenu), so it is not an
    /// overlay serf draws: that is what keeps serf chrome-free and out of the
    /// toolkit business.
    fn start_menu(&mut self, purpose: MenuPurpose, entries: String) {
        let command = self
            .shared
            .config
            .read()
            .map(|c| c.general.tab_launcher.clone())
            .unwrap_or_default();
        if command.trim().is_empty() {
            log::warn!("serf: no tab_launcher is configured, so no menu is available");
            return;
        }
        match Launcher::spawn(&command, &entries) {
            Ok(launcher) => {
                self.launcher = Some(launcher);
                self.menu_purpose = Some(purpose);
            }
            Err(e) => log::error!("serf: cannot run the launcher {command:?}: {e}"),
        }
    }

    /// Apply the running menu's answer once it closes.
    fn poll_menu(&mut self) {
        let answer = match self.launcher.as_ref() {
            Some(launcher) => launcher.poll(),
            None => return,
        };
        // `None` means the menu is still open; `Some(None)` is a cancel.
        let Some(answer) = answer else {
            return;
        };
        self.launcher = None;
        let purpose = self.menu_purpose.take();

        let Some(line) = answer else {
            // A cancelled form control still has to be answered, or the page
            // stays blocked on it forever.
            if let Some(MenuPurpose::Control(_)) = purpose {
                if let Some(engine) = self.engine.as_mut() {
                    engine.resolve_control(ControlChoice::Dismissed);
                }
            }
            return;
        };

        match purpose {
            Some(MenuPurpose::Tab) => {
                let chosen = self
                    .shared
                    .session
                    .lock()
                    .unwrap()
                    .parse_launcher_choice(&line);
                if let Some(index) = chosen {
                    // Routed through `apply` like every other tab change, so the
                    // engine is told to load what the bar now claims is active.
                    let effect = self
                        .shared
                        .session
                        .lock()
                        .unwrap()
                        .apply(Command::Select(index));
                    self.apply_effect(effect);
                }
            }
            Some(MenuPurpose::Control(request)) => {
                // Picking a directory out of a file listing descends into it
                // instead of submitting it as the answer.
                if let ControlRequest::File { .. } = request {
                    let path = line.trim();
                    if path.ends_with('/') && std::path::Path::new(path).is_dir() {
                        let entries = file_listing(path.trim_end_matches('/'));
                        self.start_menu(MenuPurpose::Control(request.clone()), entries);
                        return;
                    }
                }
                let choice = control_choice(&request, line.trim());
                if let Some(engine) = self.engine.as_mut() {
                    engine.resolve_control(choice);
                }
            }
            None => {}
        }
    }

    /// Hand a key to the page: INSERT mode, or a key the keymap does not claim.
    fn forward_key(&mut self, event: &KeyEvent) {
        let Some(key) = App::page_key(event, self.modifiers) else {
            return;
        };
        if let Some(engine) = self.engine.as_mut() {
            engine.send_key(&key);
        }
    }

    /// Describe a winit key event in the engine's key vocabulary.
    ///
    /// Separate from [`App::translate`] on purpose: that one maps onto the *vim*
    /// keymap and deliberately collapses everything it does not bind, while this
    /// one has to preserve exactly what the page needs to see.
    fn page_key(event: &KeyEvent, modifiers: ModifiersState) -> Option<PageKey> {
        let base = PageKey {
            character: None,
            named: None,
            ctrl: modifiers.contains(ModifiersState::CONTROL),
            alt: modifiers.contains(ModifiersState::ALT),
            shift: modifiers.contains(ModifiersState::SHIFT),
            logo: modifiers.contains(ModifiersState::SUPER),
            repeat: event.repeat,
            release: event.state == ElementState::Released,
        };
        match &event.logical_key {
            WinitKey::Character(text) => Some(PageKey {
                character: Some(text.to_string()),
                ..base
            }),
            WinitKey::Named(named) => Some(PageKey {
                named: Some(named_key_name(*named)?.to_string()),
                ..base
            }),
            _ => None,
        }
    }

    fn apply_effect(&mut self, effect: Effect) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        match effect {
            Effect::Navigate(url) | Effect::NewTab(url) => {
                if let Err(e) = engine.navigate(&url) {
                    log::warn!("serf: navigate failed: {e}");
                }
                self.apply_domain_rules(&url);
            }
            Effect::Reload => {
                let _ = engine.reload();
            }
            Effect::Scroll { dx, dy } => engine.scroll_by(dx, dy),
            Effect::SetMode(m) => {
                if let Some(mode) = Mode::parse(&m) {
                    self.vim = VimState::new(
                        Keymap::defaults().with_overrides(&self.shared.config.read().unwrap().keybindings),
                        mode,
                    );
                }
            }
            Effect::Hints => self.start_hints(),
            Effect::ReloadTheme => {
                // Re-read the file from disk as well as re-resolving the
                // palette, so `serf-msg theme reload` also picks up
                // keybinding edits that landed while we were running.
                if let Ok(fresh) = Config::load(&self.config_path) {
                    *self.shared.config.write().unwrap() = fresh;
                }
                let cfg = self.shared.config.read().unwrap();
                self.theme.refresh(&cfg.theme, &cfg.colors);
                self.shared.overlay.set_palette(self.theme.palette);
                self.vim.set_keymap(Keymap::defaults().with_overrides(&cfg.keybindings));
                let _ = engine.inject_css(&self.theme.page_stylesheet());
            }
            Effect::Quit => std::process::exit(0),
            Effect::None => {}
        }
        self.sync_status();
    }

    /// Apply the config's per-domain zoom and extra hide rules for `url`.
    fn apply_domain_rules(&mut self, url: &str) {
        let cfg = self.shared.config.read().unwrap();
        let host = url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap_or("");
        let mut css = self.theme.page_stylesheet();
        if let Some(rule) = cfg.domains.get(host) {
            if let Some(zoom) = rule.zoom {
                if let Some(engine) = self.engine.as_mut() {
                    engine.set_zoom(zoom);
                }
            }
            if !rule.hide.is_empty() {
                css.push_str(&rule.hide.join(",\n"));
                css.push_str(" { display: none !important; }\n");
            }
        }
        // Cosmetic adblocking for this host.
        if let Some(engine) = self.adblock.as_ref() {
            css.push_str(&engine.cosmetic_css(url, &[], &[]));
        }
        if !css.is_empty() {
            if let Some(engine) = self.engine.as_mut() {
                engine.inject_css(&css);
            }
        }
    }

    fn start_hints(&mut self) {
        let Some(engine) = self.engine.as_mut() else { return };
        let sink: LinkSink = Arc::new(Mutex::new(Vec::new()));
        engine.request_links(sink.clone());
        let links: Vec<LinkBox> = sink.lock().map(|g| g.clone()).unwrap_or_default();
        if links.is_empty() {
            log::info!("serf: no links to hint");
            return;
        }
        log::info!("serf: {} links, hinting", links.len());
        self.hints = Some(HintSession::new(&links, &Default::default()));
    }

    fn handle_hint_key(&mut self, key: Key) {
        let Some(hints) = self.hints.as_mut() else { return };
        match key {
            Key::Escape => self.hints = None,
            Key::Backspace => {
                if !hints.backspace() {
                    self.hints = None;
                }
            }
            Key::Char(c) => match hints.push(c) {
                HintOutcome::Pending => {}
                HintOutcome::NoMatch => self.hints = None,
                HintOutcome::Selected(i) => {
                    let url = hints.link_at(i).map(|h| h.link.url.clone());
                    self.hints = None;
                    if let Some(url) = url {
                        let effect = self
                            .shared
                            .session
                            .lock()
                            .unwrap()
                            .apply(Command::Open(url));
                        self.apply_effect(effect);
                    }
                }
            },
            _ => {}
        }
        self.sync_status();
    }

    fn apply_action(&mut self, action: Action) {
        let session_effect: Option<Effect> = match action {
            Action::Scroll { dx, dy } => Some(Effect::Scroll { dx, dy }),
            Action::HalfPage { direction } => {
                let page = self
                    .shared
                    .config
                    .read()
                    .map(|c| c.overlay.height as f64 * 12.0)
                    .unwrap_or(600.0);
                Some(Effect::Scroll {
                    dx: 0.0,
                    dy: direction as f64 * page,
                })
            }
            Action::ScrollTop => Some(Effect::Scroll { dx: 0.0, dy: f64::MIN }),
            Action::ScrollBottom => Some(Effect::Scroll { dx: 0.0, dy: f64::MAX }),
            Action::Hint { .. } => Some(Effect::Hints),
            Action::HistoryBack => Some(Effect::None),
            Action::HistoryForward => Some(Effect::None),
            Action::Reload => Some(Effect::Reload),
            Action::TabNext => Some(Effect::None),
            Action::TabPrev => Some(Effect::None),
            Action::TabClose => Some(Effect::None),
            Action::TabNew => Some(Effect::None),
            Action::TabMenu => Some(Effect::None),
            Action::YankUrl | Action::PasteOpen | Action::Search { .. } => Some(Effect::None),
            Action::Quit => Some(Effect::Quit),
            Action::EnterMode(_) | Action::None | Action::PassThrough => None,
            Action::OpenPrompt { .. } => None,
            Action::SubmitCommand(ref line) => {
                let (_, effect) = self.shared.session.lock().unwrap().handle_ipc(line);
                Some(effect)
            }
        };

        // Commands that also move the session model.
        match action {
            Action::HistoryBack => {
                let e = self.shared.session.lock().unwrap().apply(Command::Back);
                self.apply_effect(e);
            }
            Action::HistoryForward => {
                let e = self.shared.session.lock().unwrap().apply(Command::Forward);
                self.apply_effect(e);
            }
            // Routed through `apply` (not the bare model methods) so the engine
            // gets the Navigate effect and the shown page actually follows the
            // tab change — see navigation.rs.
            Action::TabNext => {
                let e = self.shared.session.lock().unwrap().apply(Command::Next);
                self.apply_effect(e);
            }
            Action::TabPrev => {
                let e = self.shared.session.lock().unwrap().apply(Command::Prev);
                self.apply_effect(e);
            }
            Action::TabClose => {
                let e = self.shared.session.lock().unwrap().apply(Command::Close);
                self.apply_effect(e);
            }
            Action::TabNew => {
                let url = self
                    .shared
                    .config
                    .read()
                    .map(|c| c.general.homepage.clone())
                    .unwrap_or_else(|_| "about:blank".into());
                let e = self.shared.session.lock().unwrap().apply(Command::OpenNewTab(url));
                self.apply_effect(e);
            }
            Action::TabMenu => self.open_tab_menu(),
            Action::OpenPrompt { edit } => {
                if edit {
                    let url = self.shared.session.lock().unwrap().current().current().to_string();
                    self.vim.prefill_open_url(&url);
                }
            }
            Action::PassThrough => {
                // Unreachable from the event loop, which forwards page-owned keys
                // itself (`App::forward_key`) before the action is applied. Kept
                // so the action stays total.
            }
            _ => {}
        }

        if let Some(effect) = session_effect {
            self.apply_effect(effect);
        }
        self.sync_status();
    }

    /// Translate a winit key event into serf's key vocabulary.
    fn translate(event: &KeyEvent, modifiers: ModifiersState) -> Option<Key> {
        if event.state != ElementState::Pressed {
            return None;
        }
        match &event.logical_key {
            WinitKey::Named(NamedKey::Enter) => Some(Key::Enter),
            WinitKey::Named(NamedKey::Escape) => Some(Key::Escape),
            WinitKey::Named(NamedKey::Backspace) => Some(Key::Backspace),
            WinitKey::Named(NamedKey::Tab) => Some(Key::Tab),
            WinitKey::Named(NamedKey::Space) => Some(Key::Char(' ')),
            WinitKey::Character(s) => {
                let c = s.chars().next()?;
                if modifiers.contains(ModifiersState::CONTROL) {
                    Some(Key::Ctrl(c.to_ascii_lowercase()))
                } else {
                    Some(Key::Char(c))
                }
            }
            _ => None,
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("serf")
                // Wayland app_id. Without it the toplevel has no application id
                // at all — mango labelled the window `broken` — so compositor
                // rules, taskbars and screen shares cannot match serf.
                .with_name("serf", "serf"),
        ) {
            Ok(w) => w,
            Err(e) => {
                log::error!("serf: cannot create a window: {e}");
                event_loop.exit();
                return;
            }
        };
        let homepage = self
            .shared
            .config
            .read()
            .map(|c| c.general.homepage.clone())
            .unwrap_or_else(|_| "about:blank".into());

        match create_engine(
            &homepage,
            Some(&window),
            self.wake.clone(),
            self.adblock.clone(),
            self.shared.blocked.clone(),
            self.shared.filtered.clone(),
        ) {
            Ok(engine) => self.engine = Some(engine),
            Err(e) => {
                log::error!("serf: engine init failed: {e}");
                self.engine = Some(Box::new(StubEngine::new(homepage)));
            }
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.inject_css(&self.theme.page_stylesheet());
        }
        // Ask for the first frame. A Wayland toplevel is created by `create_window`
        // but stays unmapped until a buffer is committed, and the commit happens on
        // `RedrawRequested`; Servo's own frame-ready wake drives every frame after
        // this one (`ServoWaker`), but the first request has no prior event to hang
        // off, so it is made here.
        window.request_redraw();
        self.window = Some(window);
        self.sync_status();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        if let Some(engine) = self.engine.as_mut() {
            engine.spin();
        }
        match event {
            UserEvent::Effect(effect) => self.apply_effect(*effect),
            // A wake is Servo saying there is work for it: `spin` above delivers that
            // work — a ready frame lands in the delegate's dirty flag — and
            // `poll_engine` turns it into a repaint request here and now, rather than
            // leaving it to the turn's own `about_to_wait`. The frame is the reason
            // we were woken, so it should not depend on which order winit happens to
            // run its hooks in.
            #[cfg(feature = "servo")]
            UserEvent::Wake => self.poll_engine(),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        if let Some(engine) = self.engine.as_mut() {
            engine.spin();
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Some(engine) = self.engine.as_mut() {
                    engine.present();
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(engine) = self.engine.as_mut() {
                    engine.resize(size.width, size.height);
                }
            }
            // Servo's focus flag is SYSTEM focus, not vim INSERT mode. Clearing
            // it in NORMAL prevented clicks from focusing form controls at all.
            // The keymap already keeps NORMAL/COMMAND keys away from the page.
            WindowEvent::Focused(focused) => {
                if let Some(engine) = self.engine.as_mut() {
                    engine.set_focused(focused);
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                // The keymap resolves `<C-d>`-style chords from this, so a stale
                // value silently disables every modified binding. Releasing Ctrl
                // also ends any half-finished swipe, so it cannot combine with
                // the next one.
                let state = modifiers.state();
                if !state.contains(ModifiersState::CONTROL) {
                    self.swipe.reset();
                }
                self.modifiers = state;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.hints.is_some() {
                    if let Some(key) = App::translate(&event, self.modifiers) {
                        self.handle_hint_key(key);
                    }
                } else {
                    let action =
                        App::translate(&event, self.modifiers).map(|key| self.vim.feed(key));
                    match action {
                        // The keymap claimed the key and has work to do.
                        Some(action) if action != Action::PassThrough => self.apply_action(action),
                        // INSERT: the page owns the keyboard, so an unbound key —
                        // and any key serf cannot name, like an arrow — is typed
                        // into it. In NORMAL/COMMAND an unbound key is dropped;
                        // system focus remains with the page for pointer interaction.
                        _ if self.vim.mode == Mode::Insert => self.forward_key(&event),
                        _ => {}
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                // Tracked even outside INSERT: Servo resolves a scroll against the
                // element under the point, so the vim scroll keys need it too.
                self.pointer = (position.x, position.y);
                if let Some(engine) = self.engine.as_mut() {
                    engine.send_mouse_move(position.x, position.y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let down = state == ElementState::Pressed;
                let primary = button == MouseButton::Left;
                if primary && down {
                    // Clicking a field is an explicit request to type into the
                    // page; Esc restores NORMAL. Do not conflate this with
                    // WebView system focus, which follows WindowEvent::Focused.
                    self.vim.enter_mode(Mode::Insert);
                    self.sync_status();
                }
                let (x, y) = self.pointer;
                if let Some(engine) = self.engine.as_mut() {
                    engine.send_mouse_button(down, primary, x, y);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // winit's positive `y` is "wheel up" and Servo's positive
                // `WheelDelta.y` is "the view scrolls up", so the sign carries
                // over unchanged. Only line deltas need scaling, and only because
                // Servo is told they are pixels.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (
                        x as f64 * PIXELS_PER_WHEEL_LINE,
                        y as f64 * PIXELS_PER_WHEEL_LINE,
                    ),
                    MouseScrollDelta::PixelDelta(position) => (position.x, position.y),
                };

                // A two-finger horizontal swipe is navigation, not scrolling,
                // but only while Ctrl is down: plain horizontal swipes must keep
                // panning wide pages. Ctrl = history, Ctrl+Shift = tabs.
                let gesture = self.modifiers.contains(ModifiersState::CONTROL);
                if gesture && x_dominates(dx, dy) {
                    let tabs = self.modifiers.contains(ModifiersState::SHIFT);
                    match self.swipe.push(dx, std::time::Instant::now()) {
                        Some(Swipe::Back) if tabs => self.apply_action(Action::TabPrev),
                        Some(Swipe::Forward) if tabs => self.apply_action(Action::TabNext),
                        Some(Swipe::Back) => self.apply_action(Action::HistoryBack),
                        Some(Swipe::Forward) => self.apply_action(Action::HistoryForward),
                        None => {}
                    }
                } else {
                    let (x, y) = self.pointer;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.send_wheel(dx, dy, x, y);
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // The turn hook. Both the menu and the engine are polled here rather than
        // on a wake-only path, so a menu whose child exits while nothing else is
        // happening still has its answer applied.
        if let Some(engine) = self.engine.as_mut() {
            engine.spin();
        }
        self.poll_menu();
        self.poll_engine();
    }
}

/// Where Servo keeps storage (localStorage, IndexedDB, cache storage).
///
/// Follows `XDG_DATA_HOME` like the rest of serf's state, and uses its own
/// subdirectory so a corrupt site-data store can be deleted without touching the
/// adblock filter cache next to it.
#[cfg(feature = "servo")]
fn servo_data_dir() -> Option<std::path::PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::PathBuf::from(std::env::var_os("HOME")?)
            .join(".local")
            .join("share"),
    };
    Some(base.join("serf").join("servo"))
}

/// Create the engine backend for this build.
///
/// Without the `servo` feature this is the [`StubEngine`]; with it, a real
/// Servo `WebView` is attached to the window's raw handles exactly as Servo's
/// own `examples/winit_minimal.rs` does.
///
/// `wake` is the event loop's cross-thread handle, and the servo branch hands it to
/// Servo as its [`ServoWaker`] — without it Servo's frame-ready notifications never
/// reach the loop and nothing ever paints (see that struct).
fn create_engine(
    homepage: &str,
    window: Option<&Window>,
    wake: EventLoopProxy<UserEvent>,
    adblock: Option<Rc<AdblockEngine>>,
    blocked: std::sync::Arc<std::sync::atomic::AtomicU64>,
    filtered: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<Box<dyn Engine>> {
    #[cfg(feature = "servo")]
    {
        use serf::browser::engine::servo_backend::ServoEngine;
        use servo::RenderingContext;
        use std::rc::Rc;
        use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};

        let window = window.context("the servo backend needs a winit window")?;
        let display_handle = window.display_handle().context("display handle")?;
        let window_handle = window.window_handle().context("window handle")?;
        // `surfman::error::Error` implements neither `std::error::Error` nor
        // `Display`, so anyhow's `.context()` is unavailable and `{e}` does not
        // compile — attach the message by hand and format with `{e:?}`.
        let context: Rc<dyn RenderingContext> = Rc::new(
            servo::WindowRenderingContext::new(display_handle, window_handle, window.inner_size())
                .map_err(|e| anyhow::anyhow!("creating the Servo rendering context: {e:?}"))?,
        );
        // `make_current` comes from the `RenderingContext` trait, so that trait
        // must be in scope here (it is only auto-available inside engine.rs).
        let _ = context.make_current();

        // Servo takes its storage location as a process-global option, and
        // `Servo::new` initialises that from whatever the builder carries. This
        // is therefore the only place serf can point storage at its data
        // directory: without a `config_dir` Servo's storage threads are memory
        // only, and every site forgets its `localStorage` on restart.
        let mut opts = servo::Opts::default();
        if let Some(dir) = servo_data_dir() {
            match std::fs::create_dir_all(&dir) {
                Ok(()) => {
                    log::info!("serf: servo storage in {}", dir.display());
                    opts.config_dir = Some(dir);
                }
                Err(e) => log::warn!(
                    "serf: cannot create {} ({e}); site data will not persist",
                    dir.display()
                ),
            }
        }
        opts.temporary_storage = false;

        let servo = servo::ServoBuilder::default()
            .opts(opts)
            .event_loop_waker(Box::new(ServoWaker::new(wake)))
            .build();
        servo.setup_logging();

        let engine = ServoEngine::new(servo, context, adblock, filtered, blocked);
        let url = url::Url::parse(homepage).context("invalid homepage URL")?;
        engine.create_webview(&url, window.scale_factor() as f32)?;
        Ok(Box::new(engine))
    }
    #[cfg(not(feature = "servo"))]
    {
        // The stub engine paints nothing, so neither the window nor the wake handle
        // is needed here, and it performs no network loads to filter.
        let _ = (window, wake, adblock, blocked, filtered);
        Ok(Box::new(StubEngine::new(homepage)))
    }
}

/// Servo's wake handle.
///
/// **Servo calls this from its own threads when it has work for the embedder**, and
/// a frame that is ready to paint is one of those times. It must therefore reach
/// winit: the loop blocks in its platform wait when the event queue is empty, and a
/// wake that only sets a flag (which is what this did — the body was a comment)
/// leaves that flag unread. The delegate's `notify_new_frame_ready` records
/// `DelegateState::dirty`, and a loop turn is what drains it. Without that
/// wake there is no turn to request redraws for subsequent frames, so a
/// Wayland toplevel can remain unmapped even though WebRender initialized.
/// The other half of the first-paint path is `Engine::spin`/`Engine::present`:
/// they must be implemented on the trait, not just as inherent methods of
/// `ServoEngine`, because `App` holds `Box<dyn Engine>`.
#[cfg(feature = "servo")]
#[derive(Clone)]
struct ServoWaker {
    /// winit's own cross-thread handle. It is `Send + Sync` and `send_event` is
    /// legal from any thread, which is what `EventLoopWaker` requires; Servo's own
    /// `examples/winit_minimal.rs` passes its event-loop proxy the same way.
    proxy: EventLoopProxy<UserEvent>,
}

#[cfg(feature = "servo")]
impl ServoWaker {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self { proxy }
    }
}

#[cfg(feature = "servo")]
impl servo::EventLoopWaker for ServoWaker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        if let Err(e) = self.proxy.send_event(UserEvent::Wake) {
            // The loop is gone, which happens on the way out of `run_app`; there is
            // nothing left to wake and nothing to report to.
            log::debug!("serf: cannot wake the event loop: {e}");
        }
    }
}

/// The name serf's engine vocabulary uses for a winit named key.
///
/// `None` for anything without a mapping, so an unmappable key is dropped rather
/// than sent to the page as a different one.
fn named_key_name(named: NamedKey) -> Option<&'static str> {
    Some(match named {
        NamedKey::Enter => "Enter",
        NamedKey::Escape => "Escape",
        NamedKey::Backspace => "Backspace",
        NamedKey::Tab => "Tab",
        NamedKey::Delete => "Delete",
        NamedKey::ArrowUp => "ArrowUp",
        NamedKey::ArrowDown => "ArrowDown",
        NamedKey::ArrowLeft => "ArrowLeft",
        NamedKey::ArrowRight => "ArrowRight",
        NamedKey::Home => "Home",
        NamedKey::End => "End",
        NamedKey::PageUp => "PageUp",
        NamedKey::PageDown => "PageDown",
        _ => return None,
    })
}

/// The menu entries for a form control.
///
/// Every control is answered through the same external launcher: serf has no
/// widgets, and a `<select>` or a context menu is, structurally, a pick-one list.
/// Index-carrying entries are written `N: text` so the answer maps straight back
/// onto the request, and the launcher's own filter is what makes a long list
/// usable.
fn control_entries(request: &ControlRequest) -> String {
    match request {
        ControlRequest::Select { options, .. } => options
            .iter()
            .enumerate()
            .map(|(index, label)| format!("{index}: {label}"))
            .collect::<Vec<_>>()
            .join("\n"),
        ControlRequest::ContextMenu { items } => items
            .iter()
            .enumerate()
            .map(|(index, label)| format!("{index}: {label}"))
            .collect::<Vec<_>>()
            .join("\n"),
        ControlRequest::Color { current } => {
            // A colour really wants a value prompt, and the launcher is one: every
            // dmenu-alike accepts a line that is not in the list, so any
            // `#rrggbb` works. The entries are just common starting points, with
            // the element's current value first.
            let mut entries = Vec::new();
            if let Some((r, g, b)) = current {
                entries.push(format!("#{r:02x}{g:02x}{b:02x}"));
            }
            for preset in ["#000000", "#ffffff", "#e6d7bd", "#191210", "#cf6a4c", "#3b6ea5"] {
                if !entries.iter().any(|e| e == preset) {
                    entries.push(preset.to_string());
                }
            }
            entries.join("\n")
        }
        // A file input needs an absolute path, and serf has no dialog toolkit.
        // The launcher is the path prompt: picking a directory descends into it
        // (see `App::poll_menu`), and typing a path reaches anywhere else.
        ControlRequest::File { .. } => {
            let start = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            file_listing(&start)
        }
    }
}

/// Map the launcher's answer back onto the control that asked for it.
///
/// An answer that does not fit — a stale index, an unparsable line — becomes a
/// dismissal rather than a wrong selection. The line is trimmed here so the
/// function's own contract does not depend on the caller remembering to.
fn control_choice(request: &ControlRequest, line: &str) -> ControlChoice {
    let line = line.trim();
    match request {
        ControlRequest::Select { options, .. } => match leading_index(line) {
            Some(index) if index < options.len() => ControlChoice::Selected(vec![index as u32]),
            _ => ControlChoice::Dismissed,
        },
        ControlRequest::ContextMenu { items } => match leading_index(line) {
            Some(index) if index < items.len() => ControlChoice::MenuItem(index as u32),
            _ => ControlChoice::Dismissed,
        },
        // Left raw: the engine validates the value and reports a bad one as no
        // colour at all, so a typo cannot silently become black.
        ControlRequest::Color { .. } => ControlChoice::Color(line.to_string()),
        ControlRequest::File { .. } => ControlChoice::Files(vec![line.to_string()]),
    }
}

/// The `N:` prefix that index-carrying entries are written with.
fn leading_index(line: &str) -> Option<usize> {
    line.split(':').next()?.trim().parse().ok()
}

/// One line per entry in `dir`, directories first with a trailing `/`.
///
/// Hidden entries are skipped (nobody picks `.cache` out of a menu) and the list
/// is capped, so opening a file input in a huge directory cannot flood the
/// launcher.
fn file_listing(dir: &str) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if entry.path().is_dir() {
            dirs.push(format!("{dir}/{name}/"));
        } else {
            files.push(format!("{dir}/{name}"));
        }
    }
    dirs.sort();
    files.sort();
    dirs.into_iter()
        .chain(files)
        .take(200)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod menu_tests {
    use super::*;

    #[test]
    fn a_horizontal_swipe_is_told_apart_from_a_scroll() {
        // Two fingers almost never move on one axis only.
        assert!(x_dominates(-12.0, 3.0));
        assert!(x_dominates(9.0, -2.0));
        // Mostly-vertical travel is a scroll, and a pure wheel has no x at all.
        assert!(!x_dominates(2.0, -30.0));
        assert!(!x_dominates(0.0, -30.0));
        assert!(!x_dominates(0.0, 0.0));
    }

    #[test]
    fn leading_index_reads_the_entry_prefix() {
        assert_eq!(leading_index("0: Alpha"), Some(0));
        assert_eq!(leading_index("12: Zed"), Some(12));
        assert_eq!(leading_index("  3 :spaced"), Some(3));
        assert_eq!(leading_index("no prefix here"), None);
        assert_eq!(leading_index(""), None);
    }

    #[test]
    fn a_select_answer_maps_back_to_its_index() {
        let request = ControlRequest::Select {
            options: vec!["Alpha".into(), "Beta".into()],
            size: None,
        };
        assert_eq!(
            control_choice(&request, "1: Beta"),
            ControlChoice::Selected(vec![1])
        );
        // An index that is not in the list the user was shown must not select
        // some other element: it dismisses instead.
        assert_eq!(
            control_choice(&request, "9: Ghost"),
            ControlChoice::Dismissed
        );
        assert_eq!(control_choice(&request, "garbage"), ControlChoice::Dismissed);
    }

    #[test]
    fn a_context_menu_answer_maps_to_its_action_index() {
        let request = ControlRequest::ContextMenu {
            items: vec!["Back".into(), "Reload".into()],
        };
        assert_eq!(
            control_choice(&request, "1: Reload"),
            ControlChoice::MenuItem(1)
        );
        assert_eq!(
            control_choice(&request, "5: Nope"),
            ControlChoice::Dismissed
        );
    }

    #[test]
    fn a_query_answer_is_left_raw_for_the_engine_to_validate() {
        let colour = ControlRequest::Color { current: None };
        assert_eq!(
            control_choice(&colour, " #cf6a4c "),
            ControlChoice::Color("#cf6a4c".into())
        );

        let file = ControlRequest::File { multiple: false };
        assert_eq!(
            control_choice(&file, "/home/nka/notes.md"),
            ControlChoice::Files(vec!["/home/nka/notes.md".into()])
        );
    }

    #[test]
    fn select_entries_are_index_prefixed_and_ordered() {
        let request = ControlRequest::Select {
            options: vec!["Alpha".into(), "Beta".into()],
            size: None,
        };
        assert_eq!(control_entries(&request), "0: Alpha\n1: Beta");
    }

    #[test]
    fn a_colour_menu_offers_the_current_value_first() {
        let request = ControlRequest::Color {
            current: Some((0xcf, 0x6a, 0x4c)),
        };
        let entries = control_entries(&request);
        let mut lines = entries.lines();
        assert_eq!(lines.next(), Some("#cf6a4c"));
        // The current value is not duplicated by the presets.
        assert_eq!(entries.matches("#cf6a4c").count(), 1);

        // And with no current value, only the presets remain.
        let request = ControlRequest::Color { current: None };
        assert!(!control_entries(&request).is_empty());
    }

    #[test]
    fn a_file_listing_puts_directories_first_and_hides_dotfiles() {
        let dir = std::env::temp_dir().join(format!("serf-listing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join(".hidden"), b"h").unwrap();

        let listing = file_listing(dir.to_str().unwrap());
        let lines: Vec<&str> = listing.lines().collect();
        assert_eq!(lines.len(), 3, "one dir + two files, no dotfile: {lines:?}");
        assert!(lines[0].ends_with("subdir/"), "dirs first, trailing slash");
        assert!(lines[1].ends_with("a.txt"));
        assert!(lines[2].ends_with("b.txt"));
        assert!(!listing.contains(".hidden"));

        // An unreadable directory is an empty menu, not a panic.
        assert_eq!(file_listing("/definitely/not/a/directory"), "");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn named_keys_map_only_what_the_engine_knows() {
        assert_eq!(named_key_name(NamedKey::ArrowDown), Some("ArrowDown"));
        assert_eq!(named_key_name(NamedKey::Enter), Some("Enter"));
        assert_eq!(named_key_name(NamedKey::PageUp), Some("PageUp"));
        // Something serf has no mapping for is dropped rather than mistranslated.
        assert_eq!(named_key_name(NamedKey::F35), None);
    }
}
