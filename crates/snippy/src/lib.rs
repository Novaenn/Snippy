//! Service adapters, separate from CLI routing and GTK presentation.
//!
//! The X11 observer exposes metadata only. It never requests clipboard payloads.

pub mod autostart;
pub mod ipc;
pub mod notifier;
pub mod service;
pub mod settings;
pub mod storage;
pub mod x11;
