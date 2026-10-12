//! Platform entry point for the Gliff host.
#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod clipboard;
#[cfg(target_os = "linux")]
mod discovery;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod notify;
#[cfg(target_os = "linux")]
mod rate;
#[cfg(target_os = "linux")]
mod session;
#[cfg(target_os = "linux")]
mod writer;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::main()
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    gliff_macos::server_main()
}
