//! The engine abstraction and its two backends.
//!
//! [`Engine`] is the narrow surface the rest of serf actually needs. Two
//! implementations exist:
//!
//! * [`StubEngine`] — always compiled. It tracks navigation state so the
//!   whole control path (vim keys → session → engine, IPC → session →
//!   engine) can be exercised and tested without a browser engine. It is
//!   what the default build links.
//! * `servo_backend::ServoEngine` — compiled with `--features servo`. It
//!   wraps `servo::Servo` + `servo::WebView` exactly as Servo's own
//!   `servoshell` does (see `servo 0.7` `examples/winit_minimal.rs`).

use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::input::hints::LinkBox;

/// Sink the engine fills with the page's links (link enumeration is async on
/// a real engine, so it is delivered through a shared slot).
pub type LinkSink = Arc<Mutex<Vec<LinkBox>>>;

/// A snapshot of everything the page told the embedder since the last drain.
///
/// The delegate calls back asynchronously on Servo's own threads, so it cannot
/// touch the tab model directly. It fills this instead, and the event loop
/// drains it once per turn via [`Engine::take_page_state`]. Every field is an
/// `Option` so "the page said nothing this turn" stays distinct from "the page
/// said `false`".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageState {
    /// The page navigated (or a fragment changed).
    pub url: Option<String>,
    /// The `<title>` changed.
    pub title: Option<String>,
    /// The `link:`/`mouseover:` status text changed; `None` inside means "clear".
    pub status: Option<Option<String>>,
    /// The load status changed. `true` = still loading.
    pub loading: Option<bool>,
    /// History gained or lost an entry, so back/forward availability changed.
    pub history: Option<bool>,
    /// The content process crashed, with the reason.
    pub crashed: Option<String>,
    /// The page asked for fullscreen (or gave it up).
    pub fullscreen: Option<bool>,
    /// A console message the page logged, oldest first.
    pub console: Vec<String>,
}

/// A form control the page is blocked on and the embedder must draw.
///
/// serf has no chrome, so these are answered through the external menu instead
/// of a rendered widget — see `App::poll_engine`.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlRequest {
    /// `<select>`: `options` are the display texts, in document order.
    Select {
        options: Vec<String>,
        /// `None` for a single-choice element, `Some(n)` for a size=n listbox.
        size: Option<u32>,
    },
    /// `<input type=file>`: a file dialog is needed.
    File { multiple: bool },
    /// `<input type=color>`, with the element's current colour.
    Color { current: Option<(u8, u8, u8)> },
    /// A right-click context menu, with the item labels Servo offers.
    ContextMenu { items: Vec<String> },
}

/// The answer to a [`ControlRequest`].
#[derive(Debug, Clone, PartialEq)]
pub enum ControlChoice {
    /// Indices into the request's `options`.
    Selected(Vec<u32>),
    /// Paths chosen for a file input.
    Files(Vec<String>),
    /// An `#rrggbb` colour.
    Color(String),
    /// A context-menu item, by index.
    MenuItem(u32),
    /// The user dismissed the control; the page is told the answer was empty.
    Dismissed,
}

/// A key press to hand to the page.
///
/// serf's input layer speaks its own small `Key` vocabulary, so the engine takes
/// this instead of a winit event. The Servo backend is the only place that needs
/// to know about `keyboard_types`, which is what keeps winit out of the engine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageKey {
    /// The text this key types, for a character key (`"a"`, `" "`, `"é"`).
    pub character: Option<String>,
    /// A non-character key, in serf's spelling (`"Enter"`, `"Backspace"`, ...).
    pub named: Option<String>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Super/Logo/Meta.
    pub logo: bool,
    /// An auto-repeat, not a fresh press.
    pub repeat: bool,
    /// This is a key-up, not a key-down.
    pub release: bool,
}

/// The operations serf performs on a web page.
pub trait Engine {
    /// Load `url` in the active web view.
    fn navigate(&mut self, url: &str) -> Result<()>;
    /// Reload the active web view.
    fn reload(&mut self) -> Result<()>;
    /// Go back in the web view's own history.
    fn back(&mut self) -> Result<()>;
    /// Go forward in the web view's own history.
    fn forward(&mut self) -> Result<()>;
    /// Scroll by a delta in CSS pixels. `dy == f64::MIN` means "to the top",
    /// `dy == f64::MAX` means "to the bottom".
    fn scroll_by(&mut self, dx: f64, dy: f64);
    /// Set the page zoom factor.
    fn set_zoom(&mut self, zoom: f32);
    /// Inject a user stylesheet (used for cosmetic adblocking and theming).
    fn inject_css(&mut self, css: &str);
    /// Ask the engine to enumerate the page's links into `sink`.
    fn request_links(&mut self, sink: LinkSink);
    /// The current page title (falls back to the URL).
    fn title(&self) -> String;
    /// The current page URL.
    fn url(&self) -> String;
    /// Handle a viewport resize.
    fn resize(&mut self, width: u32, height: u32);
    /// Drive the engine's event loop. The stub implements this as a no-op.
    /// Required rather than defaulted: forgetting to override it leaves Servo
    /// initialized but unable to process page or paint messages.
    fn spin(&mut self);
    /// Present the last painted frame. The stub implements this as a no-op.
    /// Required because a default no-op makes a real engine's window invisible.
    fn present(&mut self);

    // --- page interaction -------------------------------------------------
    //
    // These exist so a page is not a screenshot: a real engine forwards keys,
    // pointer and wheel events to it, and reports back what it did. The
    // defaults are all no-ops, which is exactly right for the `StubEngine`.

    /// Forward a key to the page. Returns whether the event was delivered —
    /// whether the *page* consumed it is answered asynchronously, via
    /// [`Engine::last_key_handled`].
    fn send_key(&mut self, _key: &PageKey) -> bool {
        false
    }

    /// Whether the page consumed the most recent [`Engine::send_key`].
    fn last_key_handled(&self) -> Option<bool> {
        None
    }

    /// Tell Servo whether the containing OS window has system focus. Keyboard
    /// forwarding is controlled separately by the embedder's modal keymap.
    fn set_focused(&mut self, _focused: bool) {}

    /// Forward a mouse button at viewport coordinates (device pixels).
    fn send_mouse_button(&mut self, _down: bool, _primary: bool, _x: f64, _y: f64) {}

    /// Forward a pointer move at viewport coordinates (device pixels).
    fn send_mouse_move(&mut self, _x: f64, _y: f64) {}

    /// Forward a wheel/scroll gesture. `dy > 0` scrolls the page down.
    fn send_wheel(&mut self, _dx: f64, _dy: f64, _x: f64, _y: f64) {}

    /// Drain what the page reported since the last call. `None` means nothing
    /// happened.
    fn take_page_state(&mut self) -> Option<PageState> {
        None
    }

    /// Drain the URLs the page asked to open elsewhere (`window.open`,
    /// `target="_blank"`, a link with a modifier held).
    fn take_new_tabs(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// The form control the page is blocked on, if any. The page stays blocked
    /// until [`Engine::resolve_control`] answers, so a caller that ignores this
    /// leaves a dead `<select>` on screen.
    fn pending_control(&mut self) -> Option<ControlRequest> {
        None
    }

    /// Answer [`Engine::pending_control`].
    fn resolve_control(&mut self, _choice: ControlChoice) {}

    /// Whether the engine has a fresh frame to show.
    ///
    /// The event loop asks this every turn. A real engine needs it: Servo paints
    /// on its own schedule and calls back when a frame is ready, so without this
    /// the window would only ever update on an OS-initiated redraw.
    fn wants_repaint(&mut self) -> bool {
        false
    }
}

/// The JavaScript serf runs to enumerate clickable links.
///
/// Returns a JSON array of `{url,text,x,y,w,h}` for every visible `<a href>`,
/// capped so a pathological page cannot flood the hint overlay.
pub const LINK_ENUMERATION_JS: &str = r#"
(() => {
  const out = [];
  for (const a of document.querySelectorAll('a[href]')) {
    const r = a.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    if (r.bottom < 0 || r.top > (window.innerHeight || 1e9)) continue;
    out.push({
      url: a.href,
      text: (a.innerText || a.textContent || '').trim().slice(0, 120),
      x: r.left, y: r.top, w: r.width, h: r.height
    });
    if (out.length >= 500) break;
  }
  return JSON.stringify(out);
})()
"#;

/// Parse the JSON produced by [`LINK_ENUMERATION_JS`].
pub fn parse_links(json: &str) -> Vec<LinkBox> {
    serde_json::from_str(json).unwrap_or_default()
}

/// A no-engine backend used by the default build.
///
/// It records everything the control path tells it, so navigation, hints and
/// theming can be driven and asserted without a real engine.
#[derive(Debug, Default, Clone)]
pub struct StubEngine {
    url: String,
    title: String,
    zoom: f32,
    scroll_x: f64,
    scroll_y: f64,
    /// All CSS injected so far, in order.
    pub injected_css: Vec<String>,
    /// Links a test wants `request_links` to report.
    pub links: Vec<LinkBox>,
    pub size: (u32, u32),
    /// Every key handed to `send_key`, in order.
    pub received_keys: Vec<PageKey>,
    /// Pointer and wheel events, as readable one-line descriptions.
    pub input_log: Vec<String>,
    /// State a test wants `take_page_state` to report exactly once.
    pub page_state: Option<PageState>,
    /// URLs a test wants `take_new_tabs` to report.
    pub new_tabs: Vec<String>,
    /// A control a test wants `pending_control` to report.
    pub control: Option<ControlRequest>,
    /// The answers handed to `resolve_control`, in order.
    pub control_answers: Vec<ControlChoice>,
    /// The last focus state handed to `set_focused`.
    pub focused: Option<bool>,
    /// What `last_key_handled` should report.
    pub key_handled: Option<bool>,
    /// What `wants_repaint` should report, once.
    pub repaint_hint: bool,
}

impl StubEngine {
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            title: url.clone(),
            url,
            zoom: 1.0,
            ..Default::default()
        }
    }

    pub fn scroll_position(&self) -> (f64, f64) {
        (self.scroll_x, self.scroll_y)
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }
}

impl Engine for StubEngine {
    fn navigate(&mut self, url: &str) -> Result<()> {
        self.url = url.to_string();
        if self.title.is_empty() {
            self.title = url.to_string();
        }
        Ok(())
    }

    fn reload(&mut self) -> Result<()> {
        Ok(())
    }

    fn back(&mut self) -> Result<()> {
        Ok(())
    }

    fn forward(&mut self) -> Result<()> {
        Ok(())
    }

    fn scroll_by(&mut self, dx: f64, dy: f64) {
        match dy {
            f64::MIN => {
                self.scroll_x = 0.0;
                self.scroll_y = 0.0;
            }
            f64::MAX => {
                self.scroll_y = f64::MAX; // "bottom" is unbounded in the stub
            }
            _ => {
                self.scroll_x += dx;
                self.scroll_y += dy;
            }
        }
    }

    fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom;
    }

    fn inject_css(&mut self, css: &str) {
        self.injected_css.push(css.to_string());
    }

    fn request_links(&mut self, sink: LinkSink) {
        *sink.lock().unwrap() = self.links.clone();
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.size = (width, height);
    }

    fn spin(&mut self) {}

    fn present(&mut self) {}

    fn send_key(&mut self, key: &PageKey) -> bool {
        self.received_keys.push(key.clone());
        self.key_handled.unwrap_or(false)
    }

    fn last_key_handled(&self) -> Option<bool> {
        self.key_handled
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = Some(focused);
    }

    fn send_mouse_button(&mut self, down: bool, primary: bool, x: f64, y: f64) {
        self.input_log.push(format!(
            "button {} {} @{x},{y}",
            if primary { "primary" } else { "secondary" },
            if down { "down" } else { "up" },
        ));
    }

    fn send_mouse_move(&mut self, x: f64, y: f64) {
        self.input_log.push(format!("move @{x},{y}"));
    }

    fn send_wheel(&mut self, dx: f64, dy: f64, x: f64, y: f64) {
        self.input_log.push(format!("wheel {dx},{dy} @{x},{y}"));
    }

    fn take_page_state(&mut self) -> Option<PageState> {
        self.page_state.take()
    }

    fn take_new_tabs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.new_tabs)
    }

    fn pending_control(&mut self) -> Option<ControlRequest> {
        self.control.clone()
    }

    fn resolve_control(&mut self, choice: ControlChoice) {
        self.control_answers.push(choice);
        // The page is unblocked by the answer, so nothing is pending any more.
        self.control = None;
    }

    fn wants_repaint(&mut self) -> bool {
        std::mem::take(&mut self.repaint_hint)
    }
}

/// The Servo backend.
///
/// NOTE: this module has NOT been compiled in the session that authored it —
/// pulling it in requires building SpiderMonkey from source (see the repo's
/// BUILD notes). It is written against the `servo 0.7` embedding API as used
/// by Servo's own `examples/winit_minimal.rs`, and must be treated as
/// *unverified* until `cargo build --features servo` has been run.
#[cfg(feature = "servo")]
pub mod servo_backend {
    use std::cell::RefCell;
    use std::rc::Rc;

    use anyhow::{anyhow, Result};
    // `servo` re-exports the keyboard *primitives* (Key, Code, KeyState, Location,
    // Modifiers) but NOT `KeyboardEvent`, and the two are genuinely different
    // types: servo wraps the raw one in its own struct,
    //
    //     pub struct KeyboardEvent { pub event: ::keyboard_types::KeyboardEvent }
    //
    // So naming the crate directly does NOT make them interchangeable — the raw
    // event has to be wrapped with `servo::KeyboardEvent::new` at the point of
    // use. The version is pinned to the same 0.8.3 servo requires, so cargo
    // unifies it into a single instance and the key types agree.
    use keyboard_types::KeyboardEvent as RawKeyEvent;
    use servo::{
        Code, DevicePoint, DeviceVector2D, InputEvent, Key, KeyState, Location, Modifiers,
        MouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent, NamedKey,
        RenderingContext, Scroll, Servo, WebView, WebViewBuilder, WebViewPoint, WebViewVector,
        WheelDelta, WheelEvent, WheelMode,
    };
    use url::Url;

    use crate::browser::delegate::{DelegateState, SerfDelegate};

    use super::{
        parse_links, ControlChoice, ControlRequest, Engine, LinkSink, PageKey, PageState,
        LINK_ENUMERATION_JS,
    };

    /// An [`Engine`] backed by a live Servo `WebView`.
    pub struct ServoEngine {
        servo: Servo,
        rendering_context: Rc<dyn RenderingContext>,
        webview: RefCell<Option<WebView>>,
        zoom: f32,
        injected_css: RefCell<Vec<String>>,
        /// Filled by the `WebViewDelegate` Servo holds, drained by the event loop.
        delegate: Rc<DelegateState>,
        /// Where the pointer last was, in device pixels. Servo resolves a wheel
        /// event or a click against the element under the point it is given, so
        /// this is what makes scrolling the pane under the cursor rather than an
        /// arbitrary one — and what the vim scroll keys use as their origin.
        pointer: RefCell<(f64, f64)>,
        /// The network filter, shared with the delegate that runs the hook.
        adblock: Option<Rc<crate::adblock::AdblockEngine>>,
        /// Counters the delegate bumps per request; see `serf-msg stats`.
        filtered: Arc<std::sync::atomic::AtomicU64>,
        blocked: Arc<std::sync::atomic::AtomicU64>,
    }

    impl ServoEngine {
        pub fn new(
            servo: Servo,
            rendering_context: Rc<dyn RenderingContext>,
            adblock: Option<Rc<crate::adblock::AdblockEngine>>,
            filtered: Arc<std::sync::atomic::AtomicU64>,
            blocked: Arc<std::sync::atomic::AtomicU64>,
        ) -> Self {
            Self {
                servo,
                rendering_context,
                webview: RefCell::new(None),
                zoom: 1.0,
                injected_css: RefCell::new(Vec::new()),
                delegate: Rc::new(DelegateState::default()),
                pointer: RefCell::new((0.0, 0.0)),
                adblock,
                filtered,
                blocked,
            }
        }

        /// Create the underlying `WebView` (call once the surface exists).
        pub fn create_webview(&self, url: &Url, scale: f32) -> Result<()> {
            let builder = WebViewBuilder::new(&self.servo, self.rendering_context.clone())
                .url(url.clone())
                .hidpi_scale_factor(euclid::Scale::new(scale))
                .delegate(Rc::new(SerfDelegate::new(
                    Rc::clone(&self.delegate),
                    self.adblock.clone(),
                    self.filtered.clone(),
                    self.blocked.clone(),
                )));
            let webview = builder.build();
            // Every delegate callback is filtered on this id, so only the view the
            // user is actually looking at drives the tab model; an auxiliary view
            // built for `window.open` cannot overwrite the address bar.
            self.delegate.set_main(webview.id());
            *self.webview.borrow_mut() = Some(webview);
            Ok(())
        }

        /// Remember where the pointer is, in device pixels.
        fn remember_pointer(&self, x: f64, y: f64) {
            *self.pointer.borrow_mut() = (x, y);
        }

        pub fn servo(&self) -> &Servo {
            &self.servo
        }

        fn with_webview<T>(&self, f: impl FnOnce(&WebView) -> T) -> Result<T> {
            let webview = self.webview.borrow();
            let webview = webview
                .as_ref()
                .ok_or_else(|| anyhow!("web view not created yet"))?;
            Ok(f(webview))
        }

        /// Re-apply every stylesheet handed to [`Engine::inject_css`].
        fn reapply_css(&self) -> Result<()> {
            for css in self.injected_css.borrow().iter() {
                let script = format!(
                    "(function(){{let s=document.getElementById('serf-user-css');\
                     if(!s){{s=document.createElement('style');s.id='serf-user-css';\
                     (document.head||document.documentElement).appendChild(s);}}\
                     s.textContent={};}})()",
                    serde_json::to_string(css)?
                );
                self.with_webview(|w| w.evaluate_javascript(script.clone(), |_| {}))?;
            }
            Ok(())
        }
    }

    impl Engine for ServoEngine {
        fn spin(&mut self) {
            self.servo.spin_event_loop();
        }

        fn present(&mut self) {
            if let Some(webview) = self.webview.borrow().as_ref() {
                webview.paint();
            }
            self.rendering_context.present();
        }

        fn navigate(&mut self, url: &str) -> Result<()> {
            let parsed = Url::parse(url).map_err(|e| anyhow!("invalid url {url:?}: {e}"))?;
            self.with_webview(|w| w.load(parsed.clone()))?;
            // New documents start without our stylesheet; re-inject.
            self.reapply_css()?;
            Ok(())
        }

        fn reload(&mut self) -> Result<()> {
            self.with_webview(|w| w.reload())?;
            Ok(())
        }

        fn back(&mut self) -> Result<()> {
            self.with_webview(|w| {
                if w.can_go_back() {
                    w.go_back(1);
                }
            })?;
            Ok(())
        }

        fn forward(&mut self) -> Result<()> {
            self.with_webview(|w| {
                if w.can_go_forward() {
                    w.go_forward(1);
                }
            })?;
            Ok(())
        }

        fn scroll_by(&mut self, dx: f64, dy: f64) {
            // Real scroll events, not an injected `window.scrollBy`. A script only
            // moves the document scroller, so a page with its own scrollable panes
            // (a mail list, a file view, a code editor) ignores the key entirely;
            // `notify_scroll_event` goes through the compositor, so it scrolls
            // whatever is under the point. The `dy` sentinels stay — the vim
            // keymap speaks in "top" and "bottom", and Servo has the same idea
            // natively.
            let (x, y) = *self.pointer.borrow();
            let point = device_point(x, y);
            let result = match dy {
                f64::MIN => self.with_webview(|w| w.notify_scroll_event(Scroll::Start, point)),
                f64::MAX => self.with_webview(|w| w.notify_scroll_event(Scroll::End, point)),
                _ => self.with_webview(|w| {
                    w.notify_scroll_event(
                        // Positive y reveals content below the viewport, which is
                        // what serf's positive `dy` has always meant.
                        Scroll::Delta(WebViewVector::Device(DeviceVector2D::new(
                            dx as f32, dy as f32,
                        ))),
                        point,
                    )
                }),
            };
            if let Err(e) = result {
                log::warn!("serf: scroll failed: {e}");
            }
        }

        fn set_zoom(&mut self, zoom: f32) {
            self.zoom = zoom;
            let _ = self.with_webview(|w| w.set_page_zoom(zoom));
        }

        fn inject_css(&mut self, css: &str) {
            self.injected_css.borrow_mut().push(css.to_string());
            let _ = self.reapply_css();
        }

        fn request_links(&mut self, sink: LinkSink) {
            let result = self.with_webview(|w| {
                w.evaluate_javascript(LINK_ENUMERATION_JS, move |value| {
                    if let Ok(value) = value {
                        let encoded = match value {
                            servo::JSValue::String(s) => s,
                            other => format!("{other:?}"),
                        };
                        *sink.lock().unwrap() = parse_links(&encoded);
                    }
                });
            });
            if let Err(e) = result {
                log::warn!("serf: link enumeration failed: {e}");
            }
        }

        fn title(&self) -> String {
            self.with_webview(|w| w.page_title())
                .ok()
                .flatten()
                .unwrap_or_else(|| self.url())
        }

        fn url(&self) -> String {
            self.with_webview(|w| w.url())
                .ok()
                .flatten()
                .map(|u| u.to_string())
                .unwrap_or_default()
        }

        fn resize(&mut self, width: u32, height: u32) {
            let _ = self.with_webview(|w| {
                w.resize(winit::dpi::PhysicalSize::new(width, height));
            });
        }

        // --- page interaction ---------------------------------------------

        fn send_key(&mut self, key: &PageKey) -> bool {
            let Some(event) = keyboard_event(key) else {
                return false;
            };
            // The return value only says the event was handed to Servo; whether
            // the page *consumed* it arrives later through
            // `notify_input_event_handled` and is reported by `last_key_handled`.
            // Wrapped: servo's `InputEvent::Keyboard` takes servo's own
            // `KeyboardEvent`, which is a newtype over the raw one.
            self.with_webview(|w| {
                w.notify_input_event(InputEvent::Keyboard(servo::KeyboardEvent::new(event)));
            })
            .is_ok()
        }

        fn last_key_handled(&self) -> Option<bool> {
            self.delegate.last_key_handled()
        }

        fn set_focused(&mut self, focused: bool) {
            let _ = self.with_webview(|w| w.set_focused(focused));
        }

        fn send_mouse_button(&mut self, down: bool, primary: bool, x: f64, y: f64) {
            self.remember_pointer(x, y);
            let action = if down {
                MouseButtonAction::Down
            } else {
                MouseButtonAction::Up
            };
            let button = if primary {
                MouseButton::Primary
            } else {
                MouseButton::Secondary
            };
            let point = device_point(x, y);
            let _ = self.with_webview(|w| {
                w.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                    action, button, point,
                )));
            });
        }

        fn send_mouse_move(&mut self, x: f64, y: f64) {
            self.remember_pointer(x, y);
            let point = device_point(x, y);
            let _ = self.with_webview(|w| {
                w.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point)));
            });
        }

        fn send_wheel(&mut self, dx: f64, dy: f64, x: f64, y: f64) {
            self.remember_pointer(x, y);
            // Pixel deltas: winit reports wheel lines in `LineDelta`, and
            // converting in the event loop keeps this honest for trackpads too.
            let delta = WheelDelta {
                x: dx,
                y: dy,
                z: 0.0,
                mode: WheelMode::DeltaPixel,
            };
            let point = device_point(x, y);
            let _ = self.with_webview(|w| {
                w.notify_input_event(InputEvent::Wheel(WheelEvent::new(delta, point)));
            });
        }

        fn take_page_state(&mut self) -> Option<PageState> {
            self.delegate.take_state()
        }

        fn take_new_tabs(&mut self) -> Vec<String> {
            self.delegate.take_new_tabs()
        }

        fn pending_control(&mut self) -> Option<ControlRequest> {
            self.delegate.pending_control()
        }

        fn resolve_control(&mut self, choice: ControlChoice) {
            self.delegate.resolve_control(choice);
        }

        fn wants_repaint(&mut self) -> bool {
            self.delegate.take_repaint()
        }
    }

    /// A device-pixel point, which is the coordinate space the input events use.
    fn device_point(x: f64, y: f64) -> WebViewPoint {
        WebViewPoint::Device(DevicePoint::new(x as f32, y as f32))
    }

    /// Translate serf's key description into the event Servo's script layer takes.
    ///
    /// `None` for a key serf cannot name, so an unmapped key is dropped instead
    /// of being sent as a bogus one.
    fn keyboard_event(key: &PageKey) -> Option<RawKeyEvent> {
        let logical = match (&key.character, &key.named) {
            (Some(text), _) => Key::Character(text.clone()),
            (None, Some(name)) => Key::Named(named_key(name)?),
            (None, None) => return None,
        };
        let mut modifiers = Modifiers::empty();
        if key.ctrl {
            modifiers |= Modifiers::CONTROL;
        }
        if key.alt {
            modifiers |= Modifiers::ALT;
        }
        if key.shift {
            modifiers |= Modifiers::SHIFT;
        }
        if key.logo {
            // META, not SUPER: SUPER is deprecated (marked legacy in the spec).
            modifiers |= Modifiers::META;
        }
        Some(RawKeyEvent {
            state: if key.release {
                KeyState::Up
            } else {
                KeyState::Down
            },
            key: logical,
            // serf keys come from the logical key, so the physical code is
            // genuinely unknown rather than guessed.
            code: Code::Unidentified,
            location: Location::Standard,
            modifiers,
            repeat: key.repeat,
            is_composing: false,
        })
    }

    /// The named keys serf's key vocabulary can produce.
    fn named_key(name: &str) -> Option<NamedKey> {
        Some(match name {
            "Enter" => NamedKey::Enter,
            "Escape" => NamedKey::Escape,
            "Backspace" => NamedKey::Backspace,
            "Tab" => NamedKey::Tab,
            "Delete" => NamedKey::Delete,
            "ArrowUp" => NamedKey::ArrowUp,
            "ArrowDown" => NamedKey::ArrowDown,
            "ArrowLeft" => NamedKey::ArrowLeft,
            "ArrowRight" => NamedKey::ArrowRight,
            "Home" => NamedKey::Home,
            "End" => NamedKey::End,
            "PageUp" => NamedKey::PageUp,
            "PageDown" => NamedKey::PageDown,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_engine_tracks_navigation() {
        let mut e = StubEngine::new("about:blank");
        e.navigate("https://example.com/").unwrap();
        assert_eq!(e.url(), "https://example.com/");
    }

    #[test]
    fn stub_engine_scroll_sentinels() {
        let mut e = StubEngine::new("about:blank");
        e.scroll_by(0.0, 80.0);
        assert_eq!(e.scroll_position(), (0.0, 80.0));
        e.scroll_by(0.0, 40.0);
        assert_eq!(e.scroll_position(), (0.0, 120.0));
        e.scroll_by(0.0, f64::MIN); // top
        assert_eq!(e.scroll_position(), (0.0, 0.0));
    }

    #[test]
    fn stub_engine_records_injected_css() {
        let mut e = StubEngine::new("about:blank");
        e.inject_css("a { color: red }");
        e.inject_css("b { color: blue }");
        assert_eq!(e.injected_css.len(), 2);
    }

    #[test]
    fn link_js_output_parses() {
        let json = r#"[{"url":"https://a/","text":"A","x":1.0,"y":2.0,"w":3.0,"h":4.0}]"#;
        let links = parse_links(json);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://a/");
        assert_eq!(links[0].w, 3.0);
    }

    #[test]
    fn link_js_tolerates_missing_fields_and_garbage() {
        // `text`/geometry are optional in the schema.
        let links = parse_links(r#"[{"url":"https://a/"}]"#);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].text, "");
        assert!(parse_links("not json").is_empty());
    }

    #[test]
    fn request_links_fills_the_sink() {
        let mut e = StubEngine::new("about:blank");
        e.links = parse_links(r#"[{"url":"https://x/"}]"#);
        let sink: LinkSink = Arc::new(Mutex::new(Vec::new()));
        e.request_links(sink.clone());
        assert_eq!(sink.lock().unwrap().len(), 1);
    }

    #[test]
    fn link_enumeration_js_is_wellformed_enough_to_ship() {
        // Guards against a truncated/edited constant.
        assert!(LINK_ENUMERATION_JS.contains("querySelectorAll('a[href]')"));
        assert!(LINK_ENUMERATION_JS.contains("JSON.stringify"));
    }
}
