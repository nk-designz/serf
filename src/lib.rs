//! serf — a suckless, keyboard-driven Servo browser for Wayland.
//!
//! The crate is split so that everything except the web engine itself is
//! engine-agnostic and unit-testable on a headless box:
//!
//! * [`config`] — hot-reloadable TOML configuration and shared runtime state.
//! * [`adblock`] — `adblock-rust` engine, memory-mapped filter lists and
//!   cosmetic CSS generation for the network/layout hooks.
//! * [`input`] — the modal (NORMAL/INSERT/COMMAND) keybinding state machine
//!   and the visual link-hinting generator.
//! * [`ipc`] — the `serf-msg` UNIX-domain-socket control channel.
//! * [`ui`] — the wlr-layer-shell status overlay and the live theming/pywal
//!   stylesheet injector.
//! * [`browser`] — tab/history model and the engine abstraction. The Servo
//!   backend lives behind the `servo` cargo feature; the default build links
//!   a lightweight stub so the whole tree compiles and runs without a
//!   multi-hour SpiderMonkey build.

pub mod adblock;
pub mod browser;
pub mod config;
pub mod input;
pub mod ipc;
pub mod ui;

pub use config::Config;
