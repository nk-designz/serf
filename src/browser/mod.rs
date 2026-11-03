//! The web engine: an engine-agnostic [`engine::Engine`] trait plus the
//! tab/history model it is driven from.
//!
//! [`launcher`] is the external menu every chooser goes through (tabs,
//! `<select>` options, file and colour pickers); it has no engine behind it and
//! is always compiled. [`delegate`] is the Servo-facing half and only exists
//! with the `servo` feature.

#[cfg(feature = "servo")]
pub mod cookies;
#[cfg(feature = "servo")]
pub mod delegate;
pub mod engine;
pub mod launcher;
pub mod navigation;
