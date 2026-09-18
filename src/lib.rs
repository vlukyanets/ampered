//! `ampered` — a power management daemon for Linux laptops under Wayland.
//!
//! The crate is a library so that the daemon (`src/main.rs`), the CLI
//! (`src/bin/amperedctl.rs`) and the tests can share the same types. Start
//! with `docs/01-architecture.md`; every module links to its own document.

pub mod backlight;
pub mod config;
pub mod core;
pub mod display;
pub mod idle;
pub mod ipc;
pub mod logind;
pub mod power;
pub mod timers;

/// Locks a mutex, taking the data back even if a panicking task poisoned it.
///
/// Everything behind these locks is plain state that can be read and rewritten;
/// carrying on with it beats taking the whole daemon down, and `CLAUDE.md`
/// rules out `unwrap()` outside tests.
pub fn locked<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
