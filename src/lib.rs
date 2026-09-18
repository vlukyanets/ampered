//! `ampered` — a power management daemon for Linux laptops under Wayland.
//!
//! The crate is a library so that the daemon (`src/main.rs`), the CLI
//! (`src/bin/amperedctl.rs`) and the tests can share the same types. Start
//! with `docs/01-architecture.md`; every module links to its own document.

pub mod backlight;
pub mod config;
pub mod core;
pub mod idle;
pub mod ipc;
pub mod power;
pub mod timers;
