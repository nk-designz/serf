//! The status overlay: a `wlr-layer-shell` surface drawn by hand.
//!
//! Two layers:
//!
//! * the **renderer** ([`Canvas`], [`glyph`], [`render_status`]) — pure and
//!   unit-tested; it turns a [`StatusLine`] into ARGB pixels using a built-in
//!   5x7 bitmap font (no font library, no font files — the suckless choice).
//! * the **presenter** (`run_layer_overlay`) — creates the layer surface on
//!   its own `wl_display` connection and thread, and blits the rendered
//!   buffer into an `wl_shm` pool. This keeps the overlay completely out of
//!   the window's rendering path: the page owns the whole viewport (surf's
//!   "zero-UI core"), and the status line appears as a compositor-drawn bar.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::ui::theme::{Palette, Rgb};

/// Glyph cell geometry (the classic 5x7 LCD font).
pub const GLYPH_W: u32 = 5;
pub const GLYPH_H: u32 = 7;
/// One column of padding between glyph cells.
pub const GLYPH_ADVANCE: u32 = GLYPH_W + 1;

/// A 5x7 glyph: 7 rows, the low 5 bits of each row, MSB-of-5 = leftmost pixel.
pub type Glyph = [u8; 7];

const BLANK: Glyph = [0; 7];
const BOX: Glyph = [
    0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111,
];

/// The built-in font. Coverage is the printable ASCII set needed by a browser
/// status line (letters, digits, URL punctuation); anything else renders as a
/// hollow box so bad bytes are visible rather than silent.
pub fn glyph(c: char) -> Glyph {
    match c {
        ' ' => BLANK,
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        'a' => [0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111],
        'b' => [0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b11110],
        'c' => [0b00000, 0b00000, 0b01111, 0b10000, 0b10000, 0b10000, 0b01111],
        'd' => [0b00001, 0b00001, 0b01111, 0b10001, 0b10001, 0b10001, 0b01111],
        'e' => [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110],
        'f' => [0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000],
        'g' => [0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b01110],
        'h' => [0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001],
        'i' => [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110],
        'j' => [0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100],
        'k' => [0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010],
        'l' => [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'm' => [0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10101, 0b10101],
        'n' => [0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001],
        'o' => [0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110],
        'p' => [0b00000, 0b00000, 0b11110, 0b10001, 0b11110, 0b10000, 0b10000],
        'q' => [0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b00001],
        'r' => [0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000],
        's' => [0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
        't' => [0b01000, 0b01000, 0b11100, 0b01000, 0b01000, 0b01001, 0b00110],
        'u' => [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101],
        'v' => [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'w' => [0b00000, 0b00000, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010],
        'x' => [0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001],
        'y' => [0b00000, 0b00000, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
        'z' => [0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110],
        ',' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00100, 0b01000],
        ':' => [0b00000, 0b00110, 0b00110, 0b00000, 0b00110, 0b00110, 0b00000],
        ';' => [0b00000, 0b00110, 0b00110, 0b00000, 0b00110, 0b00100, 0b01000],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        '\\' => [0b10000, 0b01000, 0b01000, 0b00100, 0b00010, 0b00010, 0b00001],
        '-' => [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
        '_' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111],
        '+' => [0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000],
        '=' => [0b00000, 0b00000, 0b11111, 0b00000, 0b11111, 0b00000, 0b00000],
        '?' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b00000, 0b00100],
        '!' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100],
        '&' => [0b01100, 0b10010, 0b10010, 0b01100, 0b10101, 0b10010, 0b01101],
        '%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        '@' => [0b01110, 0b10001, 0b10111, 0b10101, 0b10111, 0b10000, 0b01110],
        '#' => [0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010],
        '$' => [0b00100, 0b01111, 0b10100, 0b01110, 0b00101, 0b11110, 0b00100],
        '*' => [0b00000, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0b00000],
        '(' => [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
        ')' => [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
        '[' => [0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110],
        ']' => [0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110],
        '{' => [0b00110, 0b01000, 0b01000, 0b11000, 0b01000, 0b01000, 0b00110],
        '}' => [0b01100, 0b00010, 0b00010, 0b00011, 0b00010, 0b00010, 0b01100],
        '<' => [0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010],
        '>' => [0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000],
        '\'' => [0b00100, 0b00100, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
        '"' => [0b01010, 0b01010, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
        '|' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        '~' => [0b00000, 0b00000, 0b01000, 0b10101, 0b00010, 0b00000, 0b00000],
        '^' => [0b00100, 0b01010, 0b10001, 0b00000, 0b00000, 0b00000, 0b00000],
        '`' => [0b01000, 0b00100, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
        _ => BOX,
    }
}

/// A simple ARGB8888 pixel surface.
#[derive(Debug, Clone)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width as usize) * (height as usize)],
        }
    }

    pub fn clear(&mut self, color: Rgb) {
        let v = color.to_argb32(0xff);
        self.pixels.iter_mut().for_each(|p| *p = v);
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: Rgb) {
        if w == 0 || h == 0 {
            return;
        }
        let v = color.to_argb32(0xff);
        let x0 = x.max(0) as u32;
        let y0 = y.max(0) as u32;
        let x1 = (x + w as i32).min(self.width as i32).max(0) as u32;
        let y1 = (y + h as i32).min(self.height as i32).max(0) as u32;
        for row in y0..y1 {
            let base = (row as usize) * (self.width as usize);
            for col in x0..x1 {
                self.pixels[base + col as usize] = v;
            }
        }
    }

    /// Draw `text` with its top-left at `(x, y)`. Returns the advance width.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, color: Rgb, scale: u32) -> u32 {
        let scale = scale.max(1);
        let mut cx = x;
        for ch in text.chars() {
            let g = glyph(ch);
            for (row, bits) in g.iter().enumerate() {
                for col in 0..GLYPH_W {
                    if bits & (1 << (GLYPH_W - 1 - col)) != 0 {
                        self.fill_rect(
                            cx + (col * scale) as i32,
                            y + (row as u32 * scale) as i32,
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
            cx += (GLYPH_ADVANCE * scale) as i32;
        }
        (cx - x) as u32
    }

    pub fn text_width(text: &str, scale: u32) -> u32 {
        (text.chars().count() as u32) * GLYPH_ADVANCE * scale.max(1)
    }

    /// Serialise to the `wl_shm` `ARGB8888` byte layout.
    pub fn write_to(&self, out: &mut [u8]) {
        for (i, px) in self.pixels.iter().enumerate() {
            let b = px.to_le_bytes();
            let off = i * 4;
            if off + 4 <= out.len() {
                out[off..off + 4].copy_from_slice(&b);
            }
        }
    }
}

/// Everything the overlay needs to draw one status line.
#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub mode: String,
    pub title: String,
    pub url: String,
    pub pending: String,
    pub message: String,
}

impl StatusLine {
    /// Chrome stays hidden until a command prompt or unfinished chord needs it.
    pub fn requires_overlay(&self) -> bool {
        self.mode == "command" || !self.pending.is_empty()
    }

    /// `[normal] title` — the left-hand segment.
    pub fn left(&self) -> String {
        let mode = if self.mode.is_empty() { "normal" } else { &self.mode };
        format!("[{mode}] {}", self.title)
    }

    /// The right-hand segment: pending chord / message / URL.
    pub fn right(&self) -> String {
        if !self.pending.is_empty() {
            format!("{}", self.pending)
        } else if !self.message.is_empty() {
            self.message.clone()
        } else {
            self.url.clone()
        }
    }
}

/// Truncate `text` to fit `max_px`, marking the cut with `...`.
///
/// `keep_tail` is for a command line you are still typing: the end is what you
/// are looking at, so the marker goes at the front instead.
fn clamp_text(text: &str, max_px: u32, scale: u32, keep_tail: bool) -> String {
    if Canvas::text_width(text, scale) <= max_px {
        return text.to_string();
    }
    let cell = GLYPH_ADVANCE * scale.max(1);
    let chars: Vec<char> = text.chars().collect();
    let marker = "...";
    let mut budget = (max_px / cell) as usize;
    if budget <= marker.len() {
        return String::new();
    }
    budget -= marker.len();
    if keep_tail {
        let tail: String = chars[chars.len().saturating_sub(budget)..].iter().collect();
        format!("{marker}{tail}")
    } else {
        let head: String = chars[..budget.min(chars.len())].iter().collect();
        format!("{head}{marker}")
    }
}

/// Split the bar's width between the title and the right-hand field.
///
/// Drawing both segments at fixed anchors meant a long page title and a long URL
/// were painted on top of each other — which is exactly what a command prompt
/// with a long address looked like. The badge (`[command] `) always survives,
/// because it is how you know which mode is eating your keystrokes; the title
/// gives way first, then the right-hand field.
pub fn fit_segments(
    left: &str,
    right: &str,
    avail: u32,
    scale: u32,
    keep_right_tail: bool,
) -> (String, String) {
    let scale = scale.max(1);
    let gap = GLYPH_ADVANCE * scale;
    if Canvas::text_width(left, scale) + Canvas::text_width(right, scale) <= avail {
        return (left.to_string(), right.to_string());
    }

    let badge = match left.find(']') {
        Some(end) => &left[..=end],
        None => "",
    };
    let title = left[badge.len()..].trim_start();
    let badge_w = Canvas::text_width(badge, scale);

    // Everything after "[mode] " is shared: the title and the right-hand field
    // split it in proportion to what each one wants to show, so neither starves
    // the other. A greedy right-hand field would erase the page title entirely.
    let fixed = badge_w + GLYPH_ADVANCE * scale;
    let inner = avail.saturating_sub(fixed + gap);
    let want_left = Canvas::text_width(title, scale);
    let want_right = Canvas::text_width(right, scale);
    let total = want_left.saturating_add(want_right).max(1);
    let left_share = ((inner as u64 * want_left as u64) / total as u64) as u32;
    let right_share = inner.saturating_sub(left_share);

    let right = clamp_text(right, right_share, scale, keep_right_tail);
    let title = clamp_text(title, left_share, scale, false);
    let left = if title.is_empty() {
        badge.to_string()
    } else {
        format!("{badge} {title}")
    };
    (left, right)
}

/// Draw a status line into `canvas`.
///
/// The bar is filled with the palette background, the mode/title on the left
/// in the foreground colour, and the pending chord / URL right-aligned in the
/// dim colour (accent when a chord is pending, since you are mid-command).
/// Both segments are fitted to the bar first, so they cannot overlap.
pub fn render_status(canvas: &mut Canvas, palette: &Palette, status: &StatusLine, scale: u32) {
    canvas.clear(palette.background);

    let scale = scale.max(1);
    let baseline_y = ((canvas.height.saturating_sub(GLYPH_H * scale)) / 2) as i32;
    let pad = (3 * scale) as i32;
    let avail = canvas.width.saturating_sub((2 * pad) as u32);

    let (left, right) = fit_segments(
        &status.left(),
        &status.right(),
        avail,
        scale,
        !status.message.is_empty(),
    );

    // Accent mode badge.
    canvas.draw_text(pad, baseline_y, &left, palette.foreground, scale);

    // Right-aligned status.
    let right_width = Canvas::text_width(&right, scale) as i32;
    let right_x = (canvas.width as i32 - right_width - pad).max(pad);
    let right_color = if status.pending.is_empty() {
        palette.dim
    } else {
        palette.accent
    };
    canvas.draw_text(right_x, baseline_y, &right, right_color, scale);
}

/// Shared, thread-safe overlay state: the renderer thread reads it, the app
/// thread writes it.
#[derive(Debug)]
pub struct OverlayState {
    status: Mutex<StatusLine>,
    palette: Mutex<Palette>,
    visible: AtomicBool,
    dirty: AtomicBool,
    /// Wakes the Wayland presenter when status changes; without it the thread
    /// blocks in `blocking_dispatch` forever after the initial configure.
    wake: Option<OwnedFd>,
}

impl OverlayState {
    pub fn new(status: StatusLine, palette: Palette, visible: bool) -> Self {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        let wake = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) });
        Self {
            status: Mutex::new(status),
            palette: Mutex::new(palette),
            visible: AtomicBool::new(visible),
            dirty: AtomicBool::new(true),
            wake,
        }
    }

    fn signal_wake(&self) {
        if let Some(fd) = self.wake.as_ref() {
            let one = 1u64.to_ne_bytes();
            // Nonblocking eventfd coalesces repeated updates. EAGAIN only means
            // a wake is already pending, so there is nothing to retry.
            unsafe { libc::write(fd.as_raw_fd(), one.as_ptr().cast(), one.len()) };
        }
    }

    fn drain_wake(&self) -> bool {
        let Some(fd) = self.wake.as_ref() else { return false };
        let mut count = [0u8; 8];
        (unsafe { libc::read(fd.as_raw_fd(), count.as_mut_ptr().cast(), count.len()) }) == 8
    }

    fn wake_fd(&self) -> Option<i32> {
        self.wake.as_ref().map(AsRawFd::as_raw_fd)
    }

    pub fn set_status(&self, status: StatusLine) {
        if let Ok(mut guard) = self.status.lock() {
            *guard = status;
        }
        self.dirty.store(true, Ordering::Release);
        self.signal_wake();
    }

    pub fn status(&self) -> StatusLine {
        self.status.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn set_palette(&self, palette: Palette) {
        if let Ok(mut guard) = self.palette.lock() {
            *guard = palette;
        }
        self.dirty.store(true, Ordering::Release);
        self.signal_wake();
    }

    pub fn palette(&self) -> Palette {
        self.palette
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|_| Palette::catppuccin("mocha"))
    }

    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
        self.signal_wake();
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

/// The `wlr-layer-shell` presenter.
///
/// Runs on its own thread and `wl_display` connection so the overlay never
/// blocks the page's rendering. Returns an error (rather than panicking) if
/// the compositor has no `wlr-layer-shell` — serf then simply runs without a
/// status bar, which is the right degradation for a tiling setup that does
/// not implement the protocol.
pub fn run_layer_overlay(
    state: Arc<OverlayState>,
    height: u32,
    position: &str,
    scale: u32,
) -> anyhow::Result<()> {
    wayland_surface::run(state, height, position, scale)
}

/// Spawn [`run_layer_overlay`] on a background thread.
pub fn spawn_layer_overlay(
    state: Arc<OverlayState>,
    height: u32,
    position: String,
    scale: u32,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if let Err(e) = run_layer_overlay(state, height, &position, scale) {
            log::warn!("serf: status overlay unavailable: {e}");
        }
    })
}

mod wayland_surface {
    use std::os::fd::{AsFd, AsRawFd};
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::{anyhow, Context, Result};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    };
    use wayland_client::{Connection, Dispatch, QueueHandle};
    use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
        Layer, ZwlrLayerShellV1,
    };
    use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
        Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
    };

    use super::{render_status, Canvas, OverlayState};

    /// Proxies are held to keep the protocol objects alive for the lifetime of
    /// the surface, even where their Rust handle is not read again.
    #[allow(dead_code)]
    struct LayerState {
        compositor: wl_compositor::WlCompositor,
        shm: wl_shm::WlShm,
        layer_shell: ZwlrLayerShellV1,
        surface: wl_surface::WlSurface,
        layer: ZwlrLayerSurfaceV1,
        pool: Option<wl_shm_pool::WlShmPool>,
        buffer: Option<wl_buffer::WlBuffer>,
        mapping: Option<memmap2::MmapMut>,
        width: u32,
        height: u32,
        scale: u32,
        shared: Arc<OverlayState>,
        /// A wlr-layer-shell surface may not be attached to a buffer before the
        /// compositor's first `configure`; doing so is a protocol error that
        /// aborts the whole connection. Updates that arrive before that are
        /// rendered by the configure handler itself.
        configured: bool,
    }

    impl LayerState {
        fn redraw(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
            let strided = (self.width * 4) as usize;
            let size = strided * self.height as usize;

            // (Re)create the shm backing store when the size changed.
            if self.mapping.is_none() {
                let file = create_shm_file(size as u64)?;
                let mapping = unsafe { memmap2::MmapMut::map_mut(&file) }
                    .context("mmap for the overlay buffer")?;
                let pool = self.shm.create_pool(file.as_fd(), size as i32, qh, ());
                let buffer = pool.create_buffer(
                    0,
                    self.width as i32,
                    self.height as i32,
                    strided as i32,
                    wl_shm::Format::Argb8888,
                    qh,
                    (),
                );
                self.pool = Some(pool);
                self.buffer = Some(buffer);
                self.mapping = Some(mapping);
            }

            let mut canvas = Canvas::new(self.width, self.height);
            if self.shared.is_visible() {
                render_status(&mut canvas, &self.shared.palette(), &self.shared.status(), self.scale);
            } else {
                canvas.clear(crate::ui::theme::Rgb::new(0, 0, 0));
            }

            if let Some(mapping) = self.mapping.as_mut() {
                canvas.write_to(mapping);
            }
            if let Some(buffer) = self.buffer.as_ref() {
                self.surface.attach(Some(buffer), 0, 0);
                self.surface
                    .damage(0, 0, self.width as i32, self.height as i32);
                self.surface.commit();
            }
            Ok(())
        }
    }

    fn create_shm_file(size: u64) -> Result<std::fs::File> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let path = dir.join(format!("serf-overlay-{}.shm", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        file.set_len(size)?;
        // The name is only needed until the pool has the fd.
        let _ = std::fs::remove_file(&path);
        Ok(file)
    }

    /// Empty event handlers: the overlay is output-only.
    macro_rules! empty_dispatch {
        ($iface:ty) => {
            impl Dispatch<$iface, ()> for LayerState {
                fn event(
                    _state: &mut Self,
                    _proxy: &$iface,
                    _event: <$iface as wayland_client::Proxy>::Event,
                    _data: &(),
                    _conn: &Connection,
                    _qh: &QueueHandle<Self>,
                ) {
                }
            }
        };
    }

    empty_dispatch!(wl_compositor::WlCompositor);
    empty_dispatch!(wl_shm::WlShm);
    empty_dispatch!(wl_shm_pool::WlShmPool);
    empty_dispatch!(wl_buffer::WlBuffer);
    empty_dispatch!(wl_surface::WlSurface);
    empty_dispatch!(ZwlrLayerShellV1);

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for LayerState {
        fn event(
            _state: &mut Self,
            _proxy: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwlrLayerSurfaceV1, ()> for LayerState {
        fn event(
            state: &mut Self,
            _proxy: &ZwlrLayerSurfaceV1,
            event: wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Event,
            _data: &(),
            _conn: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Event;
            match event {
                Event::Configure { serial, width, height } => {
                    state.width = if width == 0 { state.width } else { width };
                    state.height = if height == 0 { state.height } else { height };
                    state.layer.ack_configure(serial);
                    state.configured = true;
                    if let Err(e) = state.redraw(qh) {
                        log::warn!("serf: overlay redraw failed: {e}");
                    }
                }
                Event::Closed => log::info!("serf: overlay closed by compositor"),
                _ => {}
            }
        }
    }

    pub fn run(shared: Arc<OverlayState>, height: u32, position: &str, scale: u32) -> Result<()> {
        let conn = Connection::connect_to_env()
            .map_err(|e| anyhow!("cannot open a Wayland display: {e}"))?;
        let (globals, mut queue) =
            registry_queue_init::<LayerState>(&conn).context("initialising wayland registry")?;
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor = globals
            .bind(&qh, 4..=6, ())
            .map_err(|e| anyhow!("compositor global unavailable: {e}"))?;
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| anyhow!("wl_shm global unavailable: {e}"))?;
        let layer_shell: ZwlrLayerShellV1 = globals
            .bind(&qh, 1..=4, ())
            .map_err(|e| anyhow!("compositor has no wlr-layer-shell: {e}"))?;

        let surface = compositor.create_surface(&qh, ());
        let layer = layer_shell.get_layer_surface(
            &surface,
            None,
            Layer::Top,
            "serf".to_string(),
            &qh,
            (),
        );

        // NB: wayland-scanner renders bitfield members in PascalCase
        // (`Anchor::Left`), not SCREAMING_CASE.
        let mut anchors = Anchor::Left | Anchor::Right;
        match position {
            "top" => anchors |= Anchor::Top,
            _ => anchors |= Anchor::Bottom,
        }
        layer.set_anchor(anchors);
        layer.set_size(0, height);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();

        let mut state = LayerState {
            compositor,
            shm,
            layer_shell,
            surface,
            layer,
            pool: None,
            buffer: None,
            mapping: None,
            width: 800,
            height,
            scale: scale.max(1),
            shared,
            configured: false,
        };

        // First round-trip delivers the initial configure.
        queue
            .blocking_dispatch(&mut state)
            .context("waiting for the initial layer configure")?;

        // Wait for either a Wayland configure/close or a status update. The
        // previous blocking_dispatch only woke for Wayland events, so typing a
        // command changed `OverlayState` but never repainted the bar. Polling
        // the eventfd keeps the thread asleep when nothing changes.
        loop {
            queue.dispatch_pending(&mut state).context("dispatching overlay events")?;
            // Before the first configure the surface must not be given a buffer:
            // a commit there is a protocol error, not a no-op. The configure
            // handler draws the current state as soon as it is allowed to.
            if state.configured && state.shared.take_dirty() {
                state.redraw(&qh)?;
            }
            queue.flush().context("flushing overlay requests")?;
            let Some(guard) = queue.prepare_read() else { continue };
            let wake_fd = state.shared.wake_fd();
            let mut fds = [
                libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: wake_fd.unwrap_or(-1), events: libc::POLLIN, revents: 0 },
            ];
            let timeout = if wake_fd.is_some() { -1 } else { 100 };
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("polling overlay event sources");
            }
            if fds[1].revents != 0 {
                state.shared.drain_wake();
            }
            if fds[0].revents != 0 {
                guard.read().context("reading Wayland overlay events")?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_is_blank_and_letters_are_not() {
        assert_eq!(glyph(' '), [0; 7]);
        assert!(glyph('A').iter().any(|r| *r != 0));
        assert!(glyph('z').iter().any(|r| *r != 0));
    }

    #[test]
    fn every_ascii_char_used_in_a_url_has_a_real_glyph() {
        let b = Palette::catppuccin("mocha").background;
        for c in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789:/.-_?#&=%+@,;'()[]<>!*~|^{}\"\\`$".chars()
        {
            assert_ne!(glyph(c), BOX, "missing glyph for {c:?}");
        }
        let _ = b;
    }

    #[test]
    fn command_and_pending_chord_require_overlay() {
        let mut status = StatusLine { mode: "normal".into(), ..Default::default() };
        assert!(!status.requires_overlay());
        status.mode = "command".into();
        assert!(status.requires_overlay());
        status.mode = "normal".into();
        status.pending = "g".into();
        assert!(status.requires_overlay());
    }

    #[test]
    fn unknown_characters_render_as_a_visible_box() {
        assert_eq!(glyph('\u{1F600}'), BOX);
        assert_eq!(glyph('\u{0}'), BOX);
    }

    #[test]
    fn draw_text_advances_and_marks_pixels() {
        let mut c = Canvas::new(64, 16);
        c.clear(Rgb::new(0, 0, 0));
        let adv = c.draw_text(0, 0, "A", Rgb::new(255, 255, 255), 1);
        assert_eq!(adv, GLYPH_ADVANCE);
        assert!(c.pixels.iter().any(|p| *p == 0xffffffff));
    }

    #[test]
    fn text_width_scales() {
        assert_eq!(Canvas::text_width("ab", 1), 2 * GLYPH_ADVANCE);
        assert_eq!(Canvas::text_width("ab", 2), 4 * GLYPH_ADVANCE);
        assert_eq!(Canvas::text_width("", 3), 0);
    }

    #[test]
    fn drawing_out_of_bounds_is_clipped_not_a_panic() {
        let mut c = Canvas::new(10, 8);
        c.clear(Rgb::new(1, 2, 3));
        c.fill_rect(-500, -500, 100, 100, Rgb::new(9, 9, 9));
        c.draw_text(-100, -100, "clipped", Rgb::new(255, 0, 0), 3);
        c.draw_text(1000, 1000, "gone", Rgb::new(255, 0, 0), 1);
        assert_eq!(c.pixels.len(), 80);
    }

    #[test]
    fn render_status_paints_the_bar_and_both_segments() {
        let palette = Palette::catppuccin("mocha");
        let mut c = Canvas::new(200, 16);
        let status = StatusLine {
            mode: "normal".into(),
            title: "Example".into(),
            url: "https://example.com/".into(),
            pending: String::new(),
            message: String::new(),
        };
        render_status(&mut c, &palette, &status, 1);

        let bg = palette.background.to_argb32(0xff);
        let fg = palette.foreground.to_argb32(0xff);
        assert!(c.pixels.iter().any(|p| *p == fg), "text was drawn");
        assert!(c.pixels.iter().any(|p| *p == bg), "bar background is present");

        // The left segment must actually land near the left edge: look for
        // foreground pixels in the left-hand ~16 columns rather than pinning
        // one exact coordinate to one exact glyph bit.
        let left_region_has_fg = (0..c.height as usize).any(|y| {
            (0..16usize).any(|x| c.pixels[y * c.width as usize + x] == fg)
        });
        assert!(left_region_has_fg, "left segment should render near the left edge");

        // ...and the right-aligned URL must put foreground-ish pixels in the
        // right-hand third of the bar.
        let right_color = palette.dim.to_argb32(0xff);
        let right_region_has_text = (0..c.height as usize).any(|y| {
            ((c.width as usize * 2 / 3)..c.width as usize)
                .any(|x| c.pixels[y * c.width as usize + x] == right_color)
        });
        assert!(right_region_has_text, "right segment should be right-aligned");
    }

    #[test]
    fn pending_chord_uses_the_accent_colour_and_shows_on_the_right() {
        let palette = Palette::catppuccin("mocha");
        let mut c = Canvas::new(200, 16);
        let status = StatusLine {
            mode: "normal".into(),
            title: "t".into(),
            url: "https://x/".into(),
            pending: "g".into(),
            message: String::new(),
        };
        render_status(&mut c, &palette, &status, 1);
        assert!(
            c.pixels.contains(&palette.accent.to_argb32(0xff)),
            "pending chord must be drawn in the accent colour"
        );
    }

    #[test]
    fn status_line_left_and_right() {
        let s = StatusLine {
            mode: "normal".into(),
            title: "Example".into(),
            url: "https://example.com/".into(),
            pending: String::new(),
            message: String::new(),
        };
        assert_eq!(s.left(), "[normal] Example");
        assert_eq!(s.right(), "https://example.com/");

        let s = StatusLine {
            pending: "g".into(),
            ..s
        };
        assert_eq!(s.right(), "g");
    }

    #[test]
    fn canvas_serialises_to_argb_bytes() {
        let mut c = Canvas::new(2, 1);
        c.clear(Rgb::new(0x11, 0x22, 0x33));
        let mut out = vec![0u8; 8];
        c.write_to(&mut out);
        assert_eq!(&out[0..4], &[0x33, 0x22, 0x11, 0xff]);
        assert_eq!(&out[4..8], &[0x33, 0x22, 0x11, 0xff]);
    }

    #[test]
    fn status_segments_are_fitted_so_they_cannot_overlap() {
        let title = "a".repeat(300);
        let url = "https://example.com/".repeat(20);
        let avail = 400;
        let (left, right) = fit_segments(&format!("[normal] {title}"), &url, avail, 1, false);
        assert!(
            Canvas::text_width(&left, 1) + GLYPH_ADVANCE + Canvas::text_width(&right, 1) <= avail,
            "left {:?} / right {:?} must fit inside {avail}px",
            Canvas::text_width(&left, 1),
            Canvas::text_width(&right, 1)
        );
        assert!(left.starts_with("[normal]"), "the mode badge must survive");
        assert!(left.ends_with("..."), "a truncated title is marked");
    }

    #[test]
    fn command_prompt_keeps_the_end_of_the_line_being_typed() {
        let cmd = format!("open https://example.com/{}", "x".repeat(200));
        let (left, right) = fit_segments("[command] Title", &cmd, 200, 1, true);
        assert!(right.starts_with("..."), "the cut is marked at the front: {right:?}");
        assert!(right.ends_with("xxxx"), "the tail you are typing stays visible");
        assert!(left.starts_with("[command]"));
        assert!(
            Canvas::text_width(&left, 1) + GLYPH_ADVANCE + Canvas::text_width(&right, 1) <= 200
        );
    }

    #[test]
    fn short_status_is_left_untouched() {
        let (left, right) = fit_segments("[normal] hi", "https://a/", 1000, 1, false);
        assert_eq!(left, "[normal] hi");
        assert_eq!(right, "https://a/");
    }

    #[test]
    fn overlay_updates_wake_the_renderer() {
        let state = OverlayState::new(
            StatusLine::default(),
            Palette::catppuccin("mocha"),
            false,
        );
        state.drain_wake();
        state.set_visible(true);
        assert!(state.drain_wake());
        assert!(!state.drain_wake());
        state.set_status(StatusLine { mode: "command".into(), ..Default::default() });
        assert!(state.drain_wake());
    }

    #[test]
    fn overlay_state_is_shareable_and_tracks_dirtiness() {
        let state = OverlayState::new(
            StatusLine::default(),
            Palette::catppuccin("mocha"),
            true,
        );
        assert!(state.take_dirty());
        assert!(!state.take_dirty());
        state.set_status(StatusLine {
            title: "hi".into(),
            ..Default::default()
        });
        assert!(state.take_dirty());
        assert_eq!(state.status().title, "hi");
        state.set_visible(false);
        assert!(!state.is_visible());
    }
}
