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
    /// Drive the engine's own event loop. No-op for backends that have none
    /// (the `StubEngine`); Servo needs this from the wake event.
    fn spin(&mut self) {}
    /// Present the last painted frame. No-op for `StubEngine`.
    fn present(&mut self) {}
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
    use servo::{RenderingContext, Servo, WebView, WebViewBuilder};
    use url::Url;

    use super::{parse_links, Engine, LinkSink, LINK_ENUMERATION_JS};

    /// An [`Engine`] backed by a live Servo `WebView`.
    pub struct ServoEngine {
        servo: Servo,
        rendering_context: Rc<dyn RenderingContext>,
        webview: RefCell<Option<WebView>>,
        zoom: f32,
        injected_css: RefCell<Vec<String>>,
    }

    impl ServoEngine {
        pub fn new(servo: Servo, rendering_context: Rc<dyn RenderingContext>) -> Self {
            Self {
                servo,
                rendering_context,
                webview: RefCell::new(None),
                zoom: 1.0,
                injected_css: RefCell::new(Vec::new()),
            }
        }

        /// Create the underlying `WebView` (call once the surface exists).
        pub fn create_webview(&self, url: &Url, scale: f32) -> Result<()> {
            let builder = WebViewBuilder::new(&self.servo, self.rendering_context.clone())
                .url(url.clone())
                .hidpi_scale_factor(euclid::Scale::new(scale));
            let webview = builder.build();
            *self.webview.borrow_mut() = Some(webview);
            Ok(())
        }

        pub fn servo(&self) -> &Servo {
            &self.servo
        }

        /// Drive Servo's event loop; call from the winit loop on every wake.
        pub fn spin(&self) {
            self.servo.spin_event_loop();
        }

        /// Present the last painted frame.
        pub fn present(&self) {
            if let Some(webview) = self.webview.borrow().as_ref() {
                webview.paint();
            }
            self.rendering_context.present();
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
            // Keyboard scrolling is expressed in page coordinates so it is
            // independent of hidpi scale and of the compositor's wheel
            // convention. `dy` sentinels mean top/bottom.
            let script = if dy == f64::MIN {
                "window.scrollTo({top:0,left:0,behavior:'instant'})".to_string()
            } else if dy == f64::MAX {
                "window.scrollTo({top:document.body.scrollHeight,behavior:'instant'})".to_string()
            } else {
                format!("window.scrollBy({{top:{dy},left:{dx},behavior:'instant'}})")
            };
            let _ = self.with_webview(|w| w.evaluate_javascript(script.clone(), |_| {}));
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
