//! The per-machine options live in the shared `gliff-settings` crate, so the
//! macOS client reads and writes the same `config.toml`. See
//! `docs/settings.md`.

pub use gliff_settings::*;
