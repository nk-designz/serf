//! Engine-level adblocking.
//!
//! This wraps `adblock-rust` (the engine Brave ships) so that it can be
//! driven from Servo's network request hooks and its layout-tree styling:
//!
//! * [`AdblockEngine::check`] answers "should this request be cancelled,
//!   redirected or rewritten?" — the network hook.
//! * [`AdblockEngine::cosmetic_css`] renders the `display:none !important`
//!   rules for a document — the cosmetic/layout hook.
//!
//! Filter lists are compiled once into a serialized engine and then
//! **memory-mapped** on subsequent starts ([`AdblockEngine::from_cache`])
//! instead of being re-parsed from text. That is what keeps the adblock
//! structures small and avoids holding both the raw list and the parse at
//! once. A plain EasyList parse costs tens of MB; the mmap'd form is read
//! straight off the page cache.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};

use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::Engine;
use anyhow::{Context, Result};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};

/// What should happen to a network request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Let it through untouched.
    Allow,
    /// Cancel the request (empty body).
    Block,
    /// Cancel and substitute the named redirect resource.
    BlockWithRedirect(String),
    /// Not blocked, but the URL must be rewritten (e.g. `removeparam`).
    Rewrite(String),
    /// Cancel and apply the given CSP header.
    BlockWithCsp(String),
}

impl Decision {
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            Decision::Block | Decision::BlockWithRedirect(_) | Decision::BlockWithCsp(_)
        )
    }
}

/// The request type strings `adblock-rust` understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Document,
    Subdocument,
    Script,
    Image,
    Stylesheet,
    Font,
    Media,
    Xhr,
    Websocket,
    Ping,
    Other,
}

impl RequestKind {
    /// Map a MIME-ish / internal type onto the filter-list vocabulary.
    pub fn from_mime(mime: &str) -> Self {
        let m = mime.to_ascii_lowercase();
        match m.as_str() {
            "text/html" | "application/xhtml+xml" | "document" => RequestKind::Document,
            "text/css" | "stylesheet" => RequestKind::Stylesheet,
            "text/javascript" | "application/javascript" | "script" => RequestKind::Script,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image" => {
                RequestKind::Image
            }
            "font/woff2" | "font/woff" | "font/ttf" | "font" => RequestKind::Font,
            "video/mp4" | "audio/mpeg" | "media" => RequestKind::Media,
            "application/json" | "application/xml" | "xhr" => RequestKind::Xhr,
            _ => RequestKind::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RequestKind::Document => "document",
            RequestKind::Subdocument => "subdocument",
            RequestKind::Script => "script",
            RequestKind::Image => "image",
            RequestKind::Stylesheet => "stylesheet",
            RequestKind::Font => "font",
            RequestKind::Media => "media",
            RequestKind::Xhr => "xhr",
            RequestKind::Websocket => "websocket",
            RequestKind::Ping => "ping",
            RequestKind::Other => "other",
        }
    }
}

/// A compiled adblock engine plus the (optional) backing mmap.
pub struct AdblockEngine {
    engine: Engine,
    /// Kept alive for the lifetime of the engine when loaded from a cache.
    _mmap: Option<Mmap>,
    enabled: bool,
    cosmetic: bool,
}

impl AdblockEngine {
    /// Compile an engine from a single list given as text.
    pub fn from_list_text(text: &str, tags: &[String]) -> Result<Self> {
        Self::from_filter_set({
            let mut set = FilterSet::new(false);
            set.add_filter_list(text.to_string(), ParseOptions::default());
            set
        }, tags)
    }

    /// Compile an engine from on-disk text filter lists.
    pub fn from_lists(lists: &[PathBuf], tags: &[String]) -> Result<Self> {
        let mut set = FilterSet::new(false);
        for path in lists {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading filter list {}", path.display()))?;
            set.add_filter_list(text, ParseOptions::default());
        }
        Self::from_filter_set(set, tags)
    }

    fn from_filter_set(set: FilterSet, tags: &[String]) -> Result<Self> {
        let mut engine = Engine::new_with_filter_set(set);
        // `$tag` rules are opt-in per tag. adblock-rust has deprecated the
        // runtime tag toggles in favour of rebuilding the engine, but for a
        // freshly built engine enabling the configured tags is exactly right,
        // so we keep it (and silence the deprecation deliberately).
        #[allow(deprecated)]
        {
            let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
            if !tag_refs.is_empty() {
                engine.enable_tags(&tag_refs);
            }
        }
        Ok(Self {
            engine,
            _mmap: None,
            enabled: true,
            cosmetic: true,
        })
    }

    /// Load a previously serialized engine by memory-mapping it.
    ///
    /// This is the low-RAM path: no text parsing, no duplicate copy of the
    /// ruleset in the heap.
    pub fn from_cache(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("opening adblock cache {}", path.display()))?;
        // SAFETY: the mapping is read-only and the engine deserializes into
        // its own owned structures; we never hand the mapping out mutably.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("mmap-ing adblock cache {}", path.display()))?;
        let mut engine = Engine::new_with_filter_set(FilterSet::new(false));
        engine
            .deserialize(&mmap)
            .map_err(|e| anyhow::anyhow!("deserializing adblock cache: {e}"))?;
        Ok(Self {
            engine,
            _mmap: Some(mmap),
            enabled: true,
            cosmetic: true,
        })
    }

    /// Prefer the mmap'd cache; fall back to compiling the text lists and
    /// writing a fresh cache for next time.
    pub fn load_or_build(lists: &[PathBuf], cache: &Path, tags: &[String]) -> Result<Self> {
        if cache.exists() {
            match Self::from_cache(cache) {
                Ok(engine) => {
                    log::info!("serf: adblock engine mapped from {}", cache.display());
                    return Ok(engine);
                }
                Err(e) => log::warn!("serf: adblock cache unusable ({e}); rebuilding"),
            }
        }
        let engine = Self::from_lists(lists, tags)?;
        if let Err(e) = engine.save_cache(cache) {
            log::warn!("serf: could not write adblock cache: {e}");
        }
        Ok(engine)
    }

    /// Serialize the compiled engine to `path` (written atomically).
    pub fn save_cache(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let bytes = self.engine.serialize();
        let tmp = path.with_extension("dat.tmp");
        std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
        Ok(())
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_cosmetic(&mut self, cosmetic: bool) {
        self.cosmetic = cosmetic;
    }

    /// The network hook: classify one request.
    pub fn check(
        &self,
        url: &str,
        source_url: &str,
        kind: RequestKind,
        method: &str,
    ) -> Decision {
        if !self.enabled {
            return Decision::Allow;
        }
        let request = match Request::new(url, source_url, kind.as_str(), method) {
            Ok(r) => r,
            // An unparseable URL is not ours to block.
            Err(_) => return Decision::Allow,
        };
        let result = self.engine.check_network_request(&request);
        if result.should_block() {
            if let Some(redirect) = result.redirect {
                return Decision::BlockWithRedirect(redirect);
            }
            if let Some(csp) = self.engine.get_csp_directives(&request) {
                return Decision::BlockWithCsp(csp);
            }
            return Decision::Block;
        }
        if let Some(rewritten) = result.rewritten_url {
            return Decision::Rewrite(rewritten);
        }
        Decision::Allow
    }

    /// Convenience: is a plain document/subresource load blocked?
    pub fn is_blocked(&self, url: &str, source_url: &str, kind: RequestKind) -> bool {
        self.check(url, source_url, kind, "get").is_blocking()
    }

    /// The cosmetic/layout hook: CSS that should be injected into `url`'s
    /// layout tree as a user stylesheet.
    ///
    /// `page_classes` / `page_ids` are the classes and ids actually present
    /// in the DOM; generic class/id rules are only emitted for those (this is
    /// how adblock-rust keeps generic rules O(page) instead of O(list)).
    pub fn cosmetic_css(
        &self,
        url: &str,
        page_classes: &[String],
        page_ids: &[String],
    ) -> String {
        if !self.enabled || !self.cosmetic {
            return String::new();
        }
        let resources = self.engine.url_cosmetic_resources(url);
        if resources.generichide
            && resources.hide_selectors.is_empty()
            && resources.exceptions.is_empty()
        {
            return String::new();
        }

        let mut selectors: Vec<String> = resources.hide_selectors.iter().cloned().collect();
        if !resources.generichide {
            selectors.extend(self.engine.hidden_class_id_selectors(
                page_classes,
                page_ids,
                &resources.exceptions,
            ));
        }
        selectors.sort();
        selectors.dedup();
        if selectors.is_empty() {
            return String::new();
        }

        // One rule with a selector list, matching how Brave emits hide rules.
        let mut css = String::with_capacity(selectors.iter().map(|s| s.len() + 2).sum::<usize>() + 40);
        css.push_str(&selectors.join(",\n"));
        css.push_str(" { display: none !important; }\n");
        css
    }

    /// Scriptlet injections required for `url`, if any.
    pub fn injected_script(&self, url: &str) -> String {
        if !self.enabled {
            return String::new();
        }
        self.engine.url_cosmetic_resources(url).injected_script
    }

    /// Number of selectors currently understood for a host (diagnostics).
    pub fn cosmetic_selector_count(&self, url: &str) -> usize {
        self.engine.url_cosmetic_resources(url).hide_selectors.len()
    }
}

/// Collect the distinct CSS classes and ids referenced by `exceptions` —
/// helper for callers that want generic rules for a known selector set.
pub fn exception_selectors(resources_like: &HashSet<String>) -> Vec<String> {
    resources_like.iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    const LIST: &str = "\
! test list
||ads.example.com^
||tracker.example.net^$script
@@||ads.example.com/allowed^
example.com##.ad-banner
example.com#@#.keep-me
";

    fn engine() -> AdblockEngine {
        AdblockEngine::from_list_text(LIST, &[]).expect("engine builds")
    }

    fn tmp_path(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("serf-adblock-{tag}-{}-{n}.dat", std::process::id()))
    }

    #[test]
    fn blocks_a_network_request() {
        let e = engine();
        assert_eq!(
            e.check(
                "https://ads.example.com/pixel.gif",
                "https://news.example.com/",
                RequestKind::Image,
                "get"
            ),
            Decision::Block
        );
    }

    #[test]
    fn honours_exception_rules() {
        let e = engine();
        // Covered by the @@ exception.
        assert_eq!(
            e.check(
                "https://ads.example.com/allowed",
                "https://news.example.com/",
                RequestKind::Document,
                "get"
            ),
            Decision::Allow
        );
    }

    #[test]
    fn respects_request_type_restrictions() {
        let e = engine();
        // tracker is only blocked for $script.
        assert_eq!(
            e.check(
                "https://tracker.example.net/a.js",
                "https://news.example.com/",
                RequestKind::Script,
                "get"
            ),
            Decision::Block
        );
        assert_eq!(
            e.check(
                "https://tracker.example.net/a.png",
                "https://news.example.com/",
                RequestKind::Image,
                "get"
            ),
            Decision::Allow
        );
    }

    #[test]
    fn unrelated_hosts_are_allowed() {
        let e = engine();
        assert_eq!(
            e.check(
                "https://cdn.example.org/x.css",
                "https://news.example.com/",
                RequestKind::Stylesheet,
                "get"
            ),
            Decision::Allow
        );
    }

    #[test]
    fn emits_cosmetic_css_for_matching_host() {
        let e = engine();
        let css = e.cosmetic_css("https://example.com/", &[], &[]);
        assert!(
            css.contains(".ad-banner") && css.contains("display: none !important"),
            "unexpected css: {css:?}"
        );
    }

    #[test]
    fn cosmetic_css_respects_exceptions() {
        let e = engine();
        // `.keep-me` is excepted by `#@#`; it must not appear in the hide rule.
        let css = e.cosmetic_css("https://example.com/", &[], &[]);
        assert!(!css.contains(".keep-me"), "exception leaked: {css:?}");
    }

    #[test]
    fn cosmetic_css_empty_for_other_hosts() {
        let e = engine();
        assert_eq!(e.cosmetic_css("https://clean.example.org/", &[], &[]), "");
    }

    #[test]
    fn disabled_engine_allows_everything() {
        let mut e = engine();
        e.set_enabled(false);
        assert_eq!(
            e.check(
                "https://ads.example.com/kill",
                "https://news.example.com/",
                RequestKind::Image,
                "get"
            ),
            Decision::Allow
        );
        assert_eq!(e.cosmetic_css("https://example.com/", &[], &[]), "");
    }

    #[test]
    fn cache_roundtrips_through_a_mmap() {
        let e = engine();
        let path = tmp_path("roundtrip");
        e.save_cache(&path).expect("save cache");

        let loaded = AdblockEngine::from_cache(&path).expect("load cache from mmap");
        assert_eq!(
            loaded.check(
                "https://ads.example.com/pixel.gif",
                "https://news.example.com/",
                RequestKind::Image,
                "get"
            ),
            Decision::Block,
            "rules survived the serialize/deserialize round trip"
        );
        assert!(loaded
            .cosmetic_css("https://example.com/", &[], &[])
            .contains(".ad-banner"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_or_build_writes_then_reuses_the_cache() {
        let list_path = tmp_path("list").with_extension("txt");
        let cache_path = tmp_path("cache");
        let mut f = File::create(&list_path).unwrap();
        f.write_all(LIST.as_bytes()).unwrap();
        drop(f);

        // First call compiles and writes the cache.
        let first = AdblockEngine::load_or_build(&[list_path.clone()], &cache_path, &[]).unwrap();
        assert!(cache_path.exists(), "cache should have been written");
        assert!(first.is_blocked(
            "https://ads.example.com/x",
            "https://news.example.com/",
            RequestKind::Image
        ));

        // Second call must take the mmap path and still block.
        let second = AdblockEngine::load_or_build(&[list_path.clone()], &cache_path, &[]).unwrap();
        assert!(second.is_blocked(
            "https://ads.example.com/x",
            "https://news.example.com/",
            RequestKind::Image
        ));

        std::fs::remove_file(&list_path).ok();
        std::fs::remove_file(&cache_path).ok();
    }

    #[test]
    fn mime_mapping_covers_the_common_cases() {
        assert_eq!(RequestKind::from_mime("text/html"), RequestKind::Document);
        assert_eq!(
            RequestKind::from_mime("application/javascript"),
            RequestKind::Script
        );
        assert_eq!(RequestKind::from_mime("image/webp"), RequestKind::Image);
        assert_eq!(RequestKind::from_mime("application/json"), RequestKind::Xhr);
        assert_eq!(RequestKind::from_mime("weird/thing"), RequestKind::Other);
    }
}
