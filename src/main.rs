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
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use serf::adblock::AdblockEngine;
use serf::browser::engine::{Engine, LinkSink, StubEngine};
use serf::browser::navigation::{Command, Effect, Session};
use serf::config::{self, Config, SharedConfig};
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
}

fn main() -> Result<()> {
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

    let config_path = match config_path {
        Some(p) => p,
        None => Config::default_path()?,
    };
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;

    // --build-adblock-cache: compile the lists, write the mmap'd engine, exit.
    if build_cache {
        return build_adblock_cache(&cfg);
    }

    let startup_url = homepage.unwrap_or_else(|| cfg.general.homepage.clone());
    let theme = Theme::resolve(&cfg.theme, &cfg.colors);

    // Compile or mmap the adblock engine.
    let adblock = if cfg.adblock.enabled {
        let lists: Vec<PathBuf> = cfg.adblock.lists.iter().map(|s| config::expand_tilde(s)).collect();
        let cache = config::expand_tilde(&cfg.adblock.cache);
        match AdblockEngine::load_or_build(&lists, &cache, &cfg.adblock.tags) {
            Ok(engine) => Some(engine),
            Err(e) => {
                log::warn!("serf: adblock disabled ({e}); continuing without it");
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

    let shared = Arc::new(Shared {
        config: store.clone(),
        session: Mutex::new(session),
        overlay: overlay.clone(),
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
    };

    event_loop.run_app(&mut app).context("running the winit event loop")?;
    drop(server);
    let _ = _watcher;
    let _ = _overlay_thread;
    Ok(())
}

fn serve_command(shared: &Arc<Shared>, proxy: &EventLoopProxy<UserEvent>, line: &str) -> Response {
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
         \x20 -c, --config <PATH>        config file (default ~/.config/serf/config.toml)\n\
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
    /// Main-thread-only: `adblock::Engine` is not `Send`.
    adblock: Option<AdblockEngine>,
    config_path: PathBuf,
}

impl App {
    fn sync_status(&self) {
        let session = self.shared.session.lock().unwrap();
        let tab = session.current();
        self.shared.overlay.set_status(StatusLine {
            mode: self.vim.mode.as_str().to_string(),
            title: tab.title.clone(),
            url: tab.current().to_string(),
            pending: self.vim.pending_display(),
            message: self.vim.command_line().to_string(),
        });
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
            Action::TabNext => {
                self.shared.session.lock().unwrap().next_tab();
                self.sync_status();
            }
            Action::TabPrev => {
                self.shared.session.lock().unwrap().prev_tab();
                self.sync_status();
            }
            Action::TabClose => {
                self.shared.session.lock().unwrap().close_active();
                self.sync_status();
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
            Action::PassThrough => {
                if let Some(engine) = self.engine.as_mut() {
                    // Insert mode: the key belongs to the page. A full
                    // implementation forwards the raw winit event; here the
                    // engine contract takes the already-sent event.
                    let _ = engine;
                }
                return;
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
        let window = match event_loop.create_window(Window::default_attributes().with_title("serf")) {
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

        match create_engine(&homepage, Some(&window)) {
            Ok(engine) => self.engine = Some(engine),
            Err(e) => {
                log::error!("serf: engine init failed: {e}");
                self.engine = Some(Box::new(StubEngine::new(homepage)));
            }
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.inject_css(&self.theme.page_stylesheet());
        }
        self.window = Some(window);
        self.sync_status();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        if let Some(engine) = self.engine.as_mut() {
            engine.spin();
        }
        match event {
            UserEvent::Effect(effect) => self.apply_effect(*effect),
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
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(key) = App::translate(&event, ModifiersState::empty()) {
                    if self.hints.is_some() {
                        self.handle_hint_key(key);
                    } else {
                        let action = self.vim.feed(key);
                        self.apply_action(action);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Create the engine backend for this build.
///
/// Without the `servo` feature this is the [`StubEngine`]; with it, a real
/// Servo `WebView` is attached to the window's raw handles exactly as Servo's
/// own `examples/winit_minimal.rs` does.
fn create_engine(homepage: &str, window: Option<&Window>) -> Result<Box<dyn Engine>> {
    #[cfg(feature = "servo")]
    {
        use serf::browser::engine::servo_backend::ServoEngine;
        use std::rc::Rc;
        use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};

        let window = window.context("the servo backend needs a winit window")?;
        let display_handle = window.display_handle().context("display handle")?;
        let window_handle = window.window_handle().context("window handle")?;
        // `surfman::error::Error` does not implement `std::error::Error`, so
        // anyhow's `.context()` cannot be used on it — attach the message by
        // hand instead.
        let context = Rc::new(
            servo::WindowRenderingContext::new(display_handle, window_handle, window.inner_size())
                .map_err(|e| anyhow::anyhow!("creating the Servo rendering context: {e}"))?,
        );
        let _ = context.make_current();

        let servo = servo::ServoBuilder::default()
            .event_loop_waker(Box::new(ServoWaker))
            .build();
        servo.setup_logging();

        let engine = ServoEngine::new(servo, context);
        let url = url::Url::parse(homepage).context("invalid homepage URL")?;
        engine.create_webview(&url, window.scale_factor() as f32)?;
        Ok(Box::new(engine))
    }
    #[cfg(not(feature = "servo"))]
    {
        let _ = window;
        Ok(Box::new(StubEngine::new(homepage)))
    }
}

/// Servo's wake handle. In the default build there is nothing to wake.
#[cfg(feature = "servo")]
#[derive(Clone)]
struct ServoWaker;

#[cfg(feature = "servo")]
impl servo::EventLoopWaker for ServoWaker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        // `App::user_event` calls `engine.spin()`, which drives Servo.
    }
}
