//! Platform entry point for the Gliff client.

#[cfg(target_os = "linux")]
mod clipboard;
#[cfg(target_os = "linux")]
mod clipboard_ui;
#[cfg(target_os = "linux")]
mod keymap;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod paintable;
#[cfg(target_os = "linux")]
mod recent;
#[cfg(target_os = "linux")]
mod theme;

#[cfg(target_os = "linux")]
fn main() -> gtk4::glib::ExitCode {
    linux::main()
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    gliff_macos::client_main()
}
