//! Live theming: palette resolution and CSS injection.
//!
//! serf's chrome is drawn from a [`Palette`]. Palettes come from one of three
//! sources, selected by `theme.source` in the config:
//!
//! * `pywal` — read `~/.cache/wal/colors.json`, so serf follows your wallpaper.
//! * `catppuccin` — a builtin flavour (`mocha`/`latte`/`frappe`/`macchiato`).
//! * `static` — the `[colors]` block in the config.
//!
//! Because the palette is only ever read through [`Theme`], it is
//! *hot-swappable*: the IPC command `theme reload` re-reads the source and
//! swaps the palette without restarting, which is what makes pywal work —
//! pywal rewrites `colors.json`, `serf-msg theme reload` picks it up.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::{Colors, Theme as ThemeConfig};

/// An 8-bit-per-channel RGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#rgb`, `#rrggbb` or the same without the leading `#`.
    pub fn from_hex(s: &str) -> Option<Rgb> {
        let s = s.trim().trim_start_matches('#');
        let parse = |slice: &str| u8::from_str_radix(slice, 16).ok();
        match s.len() {
            3 => {
                let mut it = s.chars();
                let dup = |c: char| {
                    let d = c.to_digit(16)? as u8;
                    Some(d * 16 + d)
                };
                Some(Rgb::new(
                    dup(it.next()?)?,
                    dup(it.next()?)?,
                    dup(it.next()?)?,
                ))
            }
            6 => Some(Rgb::new(parse(&s[0..2])?, parse(&s[2..4])?, parse(&s[4..6])?)),
            _ => None,
        }
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Pack into the `wl_shm` `ARGB8888` layout (little-endian: B,G,R,A).
    pub fn to_argb32(self, alpha: u8) -> u32 {
        ((alpha as u32) << 24) | ((self.r as u32) << 16) | ((self.g as u32) << 8) | self.b as u32
    }

    /// `rgb(r,g,b)` for use in injected CSS.
    pub fn to_css(self) -> String {
        format!("rgb({}, {}, {})", self.r, self.g, self.b)
    }
}

/// The five colours serf's chrome needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub background: Rgb,
    pub foreground: Rgb,
    pub accent: Rgb,
    pub dim: Rgb,
    pub alert: Rgb,
}

impl Palette {
    /// Builtin Catppuccin flavours. Unknown flavours fall back to `mocha`.
    pub fn catppuccin(flavour: &str) -> Self {
        let f = flavour.to_ascii_lowercase();
        let (bg, fg, accent, dim, alert) = match f.as_str() {
            "latte" => ("#eff1f5", "#4c4f69", "#1e66f5", "#9ca0b0", "#d20f39"),
            "frappe" => ("#303446", "#c6d0f5", "#8caaee", "#737994", "#e78284"),
            "macchiato" => ("#24273a", "#cad3f5", "#8aadf4", "#6e738d", "#ed8796"),
            _ => ("#1e1e2e", "#cdd6f4", "#89b4fa", "#6c7086", "#f38ba8"),
        };
        Self::from_hexes(bg, fg, accent, dim, alert)
    }

    fn from_hexes(bg: &str, fg: &str, accent: &str, dim: &str, alert: &str) -> Self {
        Self {
            background: Rgb::from_hex(bg).unwrap(),
            foreground: Rgb::from_hex(fg).unwrap(),
            accent: Rgb::from_hex(accent).unwrap(),
            dim: Rgb::from_hex(dim).unwrap(),
            alert: Rgb::from_hex(alert).unwrap(),
        }
    }

    /// Build straight from the config's `[colors]` block.
    pub fn from_config_colors(c: &Colors) -> Self {
        let get = |s: &str, fallback: Rgb| Rgb::from_hex(s).unwrap_or(fallback);
        let mocha = Palette::catppuccin("mocha");
        Self {
            background: get(&c.background, mocha.background),
            foreground: get(&c.foreground, mocha.foreground),
            accent: get(&c.accent, mocha.accent),
            dim: get(&c.dim, mocha.dim),
            alert: get(&c.alert, mocha.alert),
        }
    }

    /// Read a pywal `colors.json`.
    pub fn from_pywal(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading pywal palette {}", path.display()))?;
        let json: PywalColors =
            serde_json::from_str(&text).context("parsing pywal colors.json")?;
        let special = json.special.unwrap_or_default();
        let colors = json.colors.unwrap_or_default();
        let mocha = Palette::catppuccin("mocha");
        let pick = |s: Option<&String>, fallback: Rgb| {
            s.and_then(|s| Rgb::from_hex(s)).unwrap_or(fallback)
        };
        Ok(Self {
            background: pick(special.background.as_ref(), mocha.background),
            foreground: pick(special.foreground.as_ref(), mocha.foreground),
            accent: pick(colors.color4.as_ref(), mocha.accent),
            dim: pick(colors.color8.as_ref(), mocha.dim),
            alert: pick(colors.color1.as_ref(), mocha.alert),
        })
    }

    /// The default pywal file location.
    pub fn pywal_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/wal/colors.json"))
    }
}

#[derive(Debug, Deserialize, Default)]
struct PywalColors {
    #[serde(default)]
    special: Option<PywalSpecial>,
    #[serde(default)]
    colors: Option<PywalAnsi>,
}

#[derive(Debug, Deserialize, Default)]
struct PywalSpecial {
    background: Option<String>,
    foreground: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct PywalAnsi {
    color1: Option<String>,
    color4: Option<String>,
    color8: Option<String>,
}

/// A resolved, live theme.
#[derive(Debug, Clone)]
pub struct Theme {
    pub palette: Palette,
    pub inject_css: bool,
    pub source: String,
}

impl Theme {
    /// Resolve a theme from the config, falling back to Catppuccin if pywal
    /// is unavailable (never fail to start over a missing palette).
    pub fn resolve(cfg: &ThemeConfig, colors: &Colors) -> Self {
        let palette = match cfg.source.as_str() {
            "pywal" => match Palette::pywal_path().map(|p| Palette::from_pywal(&p)) {
                Some(Ok(p)) => p,
                Some(Err(e)) => {
                    log::warn!("serf: pywal palette unavailable ({e}); using config colours");
                    Palette::from_config_colors(colors)
                }
                None => Palette::from_config_colors(colors),
            },
            "static" => Palette::from_config_colors(colors),
            _ => Palette::catppuccin(&cfg.catppuccin_flavour),
        };
        Self {
            palette,
            inject_css: cfg.inject_css,
            source: cfg.source.clone(),
        }
    }

    /// Re-read the palette from its source (used by `theme reload`).
    pub fn refresh(&mut self, cfg: &ThemeConfig, colors: &Colors) {
        *self = Self::resolve(cfg, colors);
    }

    /// CSS for serf's own chrome (the status overlay document).
    pub fn overlay_stylesheet(&self) -> String {
        let p = self.palette;
        format!(
            ":root {{\n  --bg: {bg};\n  --fg: {fg};\n  --accent: {accent};\n  --dim: {dim};\n  --alert: {alert};\n}}\n\
             #serf-overlay {{\n  background: {bg};\n  color: {fg};\n}}\n\
             #serf-overlay .accent {{ color: {accent}; }}\n\
             #serf-overlay .dim {{ color: {dim}; }}\n\
             #serf-overlay .alert {{ color: {alert}; }}\n\
             .serf-hint {{\n  background: {accent};\n  color: {bg};\n  font-weight: bold;\n}}\n",
            bg = p.background.to_css(),
            fg = p.foreground.to_css(),
            accent = p.accent.to_css(),
            dim = p.dim.to_css(),
            alert = p.alert.to_css(),
        )
    }

    /// A user-agent stylesheet tinting page chrome (scrollbars, selection,
    /// focus rings) to match the palette. Injected only when `inject_css`.
    pub fn page_stylesheet(&self) -> String {
        if !self.inject_css {
            return String::new();
        }
        let p = self.palette;
        format!(
            ":root {{ color-scheme: {}; }}\n\
             ::selection {{ background: {accent}; color: {bg}; }}\n\
             ::-webkit-scrollbar {{ background: {bg}; width: 10px; }}\n\
             ::-webkit-scrollbar-thumb {{ background: {dim}; }}\n\
             * {{ scrollbar-color: {dim} {bg}; }}\n",
            if p.background.r as u32 + p.background.g as u32 + p.background.b as u32
                > 3 * 128
            {
                "light"
            } else {
                "dark"
            },
            bg = p.background.to_css(),
            accent = p.accent.to_css(),
            dim = p.dim.to_css(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_hex_in_all_forms() {
        assert_eq!(Rgb::from_hex("#ff0000"), Some(Rgb::new(255, 0, 0)));
        assert_eq!(Rgb::from_hex("00ff00"), Some(Rgb::new(0, 255, 0)));
        assert_eq!(Rgb::from_hex("#0f0"), Some(Rgb::new(0, 255, 0)));
        assert_eq!(Rgb::from_hex("#1e1e2e"), Some(Rgb::new(0x1e, 0x1e, 0x2e)));
        assert_eq!(Rgb::from_hex("nope"), None);
        assert_eq!(Rgb::from_hex("#12345"), None);
    }

    #[test]
    fn argb32_packing_is_little_endian_argb() {
        // wl_shm ARGB8888 on a little-endian host stores bytes B,G,R,A.
        let c = Rgb::new(0x11, 0x22, 0x33);
        let v = c.to_argb32(0xff);
        let bytes = v.to_le_bytes();
        assert_eq!(bytes, [0x33, 0x22, 0x11, 0xff]);
    }

    #[test]
    fn catppuccin_flavours_are_distinct_and_valid() {
        let mocha = Palette::catppuccin("mocha");
        let latte = Palette::catppuccin("latte");
        assert_ne!(mocha.background, latte.background);
        assert_eq!(mocha.background.to_hex(), "#1e1e2e");
        assert_eq!(mocha.accent.to_hex(), "#89b4fa");
        // unknown flavour falls back to mocha
        assert_eq!(Palette::catppuccin("banana"), mocha);
    }

    #[test]
    fn reads_a_pywal_palette() {
        let path = std::env::temp_dir().join(format!("serf-wal-{}.json", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r##"{{"special":{{"background":"#101010","foreground":"#eeeeee"}},
                "colors":{{"color1":"#ff5555","color4":"#55aaff","color8":"#444444"}}}}"##
        )
        .unwrap();
        drop(f);

        let p = Palette::from_pywal(&path).expect("pywal palette");
        assert_eq!(p.background, Rgb::new(0x10, 0x10, 0x10));
        assert_eq!(p.foreground, Rgb::new(0xee, 0xee, 0xee));
        assert_eq!(p.accent, Rgb::new(0x55, 0xaa, 0xff));
        assert_eq!(p.dim, Rgb::new(0x44, 0x44, 0x44));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_dispatches_on_source() {
        let mut cfg = ThemeConfig::default();
        cfg.source = "static".into();
        let colors = Colors {
            background: "#000000".into(),
            foreground: "#ffffff".into(),
            accent: "#123456".into(),
            ..Colors::default()
        };
        let t = Theme::resolve(&cfg, &colors);
        assert_eq!(t.palette.accent, Rgb::new(0x12, 0x34, 0x56));

        cfg.source = "catppuccin".into();
        cfg.catppuccin_flavour = "latte".into();
        let t = Theme::resolve(&cfg, &colors);
        assert_eq!(t.palette.background.to_hex(), "#eff1f5");
    }

    #[test]
    fn overlay_stylesheet_uses_the_palette() {
        let t = Theme {
            palette: Palette::catppuccin("mocha"),
            inject_css: true,
            source: "catppuccin".into(),
        };
        let css = t.overlay_stylesheet();
        assert!(css.contains("rgb(30, 30, 46)"), "{css}");
        assert!(css.contains("serf-hint"));
    }

    #[test]
    fn page_stylesheet_can_be_disabled() {
        let t = Theme {
            palette: Palette::catppuccin("mocha"),
            inject_css: false,
            source: "catppuccin".into(),
        };
        assert_eq!(t.page_stylesheet(), "");
    }

    #[test]
    fn page_stylesheet_chooses_color_scheme() {
        let dark = Theme {
            palette: Palette::catppuccin("mocha"),
            inject_css: true,
            source: "catppuccin".into(),
        };
        assert!(dark.page_stylesheet().contains("color-scheme: dark"));

        let light = Theme {
            palette: Palette::catppuccin("latte"),
            inject_css: true,
            source: "catppuccin".into(),
        };
        assert!(light.page_stylesheet().contains("color-scheme: light"));
    }
}
