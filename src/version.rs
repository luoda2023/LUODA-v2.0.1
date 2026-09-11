//! Version metadata for the LDesk binary.
//!
//! `VERSION` is derived from `Cargo.toml` at compile time. It used to be a
//! hand-maintained literal, which silently drifted: `Cargo.toml` said `2.2.40`
//! while `VERSION` still said `2.2.33`, so every 2.2.40 build (Windows, Android,
//! and the About dialog / `--version` output) self-reported `2.2.33`, and the
//! in-app updater compared against a stale number.
//!
//! Do NOT reintroduce a literal here, and do NOT resurrect
//! `hbb_common::gen_version()` (removed for the same reason — it rewrote this
//! file from `Cargo.toml` but was never wired into any build script, so it could
//! only ever add drift).

/// Application version. Single source of truth: `Cargo.toml` `[package].version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build timestamp, `YYYY-MM-DD HH:MM` in local time, injected by `build.rs`
/// via `cargo:rustc-env=LDESK_BUILD_DATE=...`.
#[allow(dead_code)]
pub const BUILD_DATE: &str = match option_env!("LDESK_BUILD_DATE") {
    Some(v) => v,
    None => "unknown",
};
