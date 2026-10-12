//! Native macOS UI, display capture and input over the existing Gliff wire.
#![forbid(unsafe_code)]

pub mod discovery;
pub mod keys;
pub mod native;

#[cfg(any(target_os = "macos", test))]
mod encoder;
#[cfg(any(target_os = "macos", test))]
mod host_bridge;

#[cfg(target_os = "macos")]
mod client;
#[cfg(target_os = "macos")]
mod clipboard;
#[cfg(target_os = "macos")]
mod server;

#[cfg(target_os = "macos")]
pub use client::client_main;
#[cfg(target_os = "macos")]
pub use server::server_main;
