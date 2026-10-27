//! gliff-mac: a native macOS client for gliff. It shows the Hyprland session
//! of another machine in a window on your Mac, over nothing but ssh.
//!
//! ```sh
//! gliff-mac user@host                 # mirror the focused remote screen
//! gliff-mac --output DP-1 user@host   # mirror a named screen
//! gliff-mac --headless user@host      # a private screen sized to this window
//! ```
//!
//! Layout: [`session`] (portable worker), [`geometry`], [`keys`],
//! [`hotkey`] (pure, tested on any OS), [`dump`] (headless PNG mode),
//! [`logging`], and `mac` (AppKit window; macOS only).

//= Off macOS only the headless pieces run; the window helpers stay compiled
//= (and tested) but unused, which is expected rather than dead code.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

mod dump;
mod geometry;
mod hotkey;
mod keys;
mod logging;
#[cfg(target_os = "macos")]
mod mac;
mod session;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use gliff_transport::SshTarget;

use crate::hotkey::ReleaseHotkey;
use crate::keys::{CommandAs, KeyOptions};
use crate::logging::LogOptions;
use crate::session::{ChromaPref, Endpoint, WorkerConfig};

//= Application version, from Cargo.toml; see CHANGELOG.md.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Clone, Debug)]
#[command(
    name = "gliff-mac",
    version,
    about = "View and drive a remote Hyprland session from macOS, over ssh"
)]
pub struct Cli {
    /// `user@host` (or an ssh config alias) running `gliff-server`. On macOS,
    /// leaving it out (e.g. opening Gliff.app from Finder) asks for one.
    pub host: Option<String>,
    /// Dev: connect straight to a `gliff-server --listen` address instead of ssh.
    #[arg(long, conflicts_with = "host")]
    pub connect: Option<String>,
    /// Remote gliff-server path (may include server flags, e.g.
    /// `'gliff-server --low-bandwidth'`).
    #[arg(long, default_value = "gliff-server")]
    pub server_bin: String,
    /// Extra ssh option, repeatable (e.g. `--ssh-opt=-J --ssh-opt=jumphost`).
    #[arg(long = "ssh-opt", allow_hyphen_values = true)]
    pub ssh_opts: Vec<String>,
    /// A private headless remote screen sized to this window.
    #[arg(long, conflicts_with = "output")]
    pub headless: bool,
    /// Mirror the named remote screen (e.g. `DP-1`) instead of the focused one.
    #[arg(long)]
    pub output: Option<String>,
    /// XKB keymap file to send, so the remote types your layout (create one
    /// on Linux with `xkbcli compile-keymap --layout de > de.xkb`). Also
    /// read from `GLIFF_KEYMAP`. Without it the remote uses US.
    #[arg(long)]
    pub keymap: Option<PathBuf>,
    /// What the Command key sends: `super` (Hyprland's usual mod), `ctrl`, `alt`.
    #[arg(long, default_value = "super")]
    pub command_as: CommandAs,
    /// Swap the § and ` keys, for Apple ISO keyboards.
    #[arg(long)]
    pub iso_swap: bool,
    /// Key that hands the keyboard back to macOS: a chord like
    /// `shift+escape` or `ctrl+alt+q`, `double-escape`, or `none`.
    #[arg(long, default_value = "shift+escape")]
    pub release_hotkey: String,
    /// Colour detail to ask for: `full` (4:4:4, sharp coloured text) or
    /// `reduced` (4:2:0, half the decode work).
    #[arg(long, default_value = "full")]
    pub chroma: ChromaPref,
    /// Do not reconnect when the link drops.
    #[arg(long)]
    pub no_reconnect: bool,
    /// Headless test mode: write decoded frames as PNG into this directory
    /// and exit (no window; works on any OS).
    #[arg(long, value_name = "DIR")]
    pub dump: Option<PathBuf>,
    /// Frames to write in `--dump` mode.
    #[arg(long, default_value_t = 1, requires = "dump")]
    pub frames: u32,
    /// Seconds to wait for the frames in `--dump` mode.
    #[arg(long, default_value_t = 30, requires = "dump")]
    pub dump_timeout: u64,
    /// Verbose logging (same as `RUST_LOG=debug`).
    #[arg(long)]
    pub debug: bool,
    /// Also append logs to this file.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,
    /// Also send logs to syslog (the macOS unified log).
    #[arg(long)]
    pub syslog: bool,
}

impl Cli {
    /// The server endpoint these flags describe.
    pub fn endpoint(&self) -> anyhow::Result<Endpoint> {
        if let Some(addr) = &self.connect {
            return Ok(Endpoint::Tcp(addr.clone()));
        }
        let host = self.host.clone().context("no host given")?;
        validate_host(&host)?;
        let mut t = SshTarget::new(host);
        t.server_bin = self.server_bin.clone();
        t.ssh_args = self.ssh_opts.clone();
        t.server_args = match (&self.output, self.headless) {
            (Some(name), _) => vec!["--output".into(), name.clone()],
            (None, true) => vec!["--headless".into()],
            (None, false) => vec!["--output".into(), "auto".into()],
        };
        Ok(Endpoint::Ssh(t))
    }

    pub fn worker_config(&self) -> anyhow::Result<WorkerConfig> {
        Ok(WorkerConfig {
            endpoint: self.endpoint()?,
            keymap: read_keymap(
                self.keymap
                    .clone()
                    .or_else(|| std::env::var_os("GLIFF_KEYMAP").map(PathBuf::from))
                    .as_deref(),
            )?,
            chroma: self.chroma,
            reconnect: !self.no_reconnect,
        })
    }

    pub fn key_options(&self) -> KeyOptions {
        KeyOptions {
            command_as: self.command_as,
            iso_swap: self.iso_swap,
        }
    }
}

/// Refuse hosts ssh would read as options or that hold whitespace/control
/// characters (`SshTarget` also puts `--` before the host).
fn validate_host(host: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!host.is_empty(), "empty host");
    anyhow::ensure!(!host.starts_with('-'), "host `{host}` looks like an option");
    anyhow::ensure!(
        !host.chars().any(|c| c.is_whitespace() || c.is_control()),
        "host `{host}` contains whitespace or control characters"
    );
    Ok(())
}

/// Read an XKB keymap file (bounded; the frame body limit applies to Hello).
fn read_keymap(path: Option<&std::path::Path>) -> anyhow::Result<String> {
    const MAX_KEYMAP: u64 = 512 * 1024;
    let Some(path) = path else {
        return Ok(String::new());
    };
    let meta = std::fs::metadata(path).with_context(|| format!("keymap {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "keymap {} is not a file", path.display());
    anyhow::ensure!(
        meta.len() <= MAX_KEYMAP,
        "keymap {} is larger than {MAX_KEYMAP} bytes",
        path.display()
    );
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read keymap {}", path.display()))?;
    anyhow::ensure!(
        text.contains("xkb_keymap"),
        "keymap {} is not XKB text (no xkb_keymap block)",
        path.display()
    );
    Ok(text)
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if let Some(note) = logging::init(&LogOptions {
        debug: cli.debug,
        file: cli.log_file.clone(),
        syslog: cli.syslog,
    }) {
        eprintln!("gliff-mac: {note}");
    }
    tracing::info!(version = VERSION, "gliff-mac starting");
    tracing::debug!(
        session = session::SESSION_VERSION,
        geometry = geometry::GEOMETRY_VERSION,
        keys = keys::KEYMAP_VERSION,
        hotkey = hotkey::HOTKEY_VERSION,
        dump = dump::DUMP_VERSION,
        logging = logging::LOGGING_VERSION,
        "module versions"
    );
    match real_main(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "fatal");
            eprintln!("gliff-mac: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn real_main(mut cli: Cli) -> anyhow::Result<()> {
    let hotkey = ReleaseHotkey::parse(&cli.release_hotkey)
        .map_err(|e| anyhow::anyhow!("--release-hotkey: {e}"))?;
    if cli.host.is_none() && cli.connect.is_none() {
        cli.host = Some(ask_host()?);
    }
    let cfg = cli.worker_config()?;
    if let Some(dir) = &cli.dump {
        return dump::run(cfg, dir, cli.frames, Duration::from_secs(cli.dump_timeout));
    }
    run_window(cfg, cli.key_options(), hotkey)
}

/// No host on the command line: ask in a dialog (macOS) or fail.
#[cfg(target_os = "macos")]
fn ask_host() -> anyhow::Result<String> {
    mac::ask_host()?.context("no host chosen")
}

#[cfg(not(target_os = "macos"))]
fn ask_host() -> anyhow::Result<String> {
    anyhow::bail!("no host given; usage: gliff-mac user@host")
}

#[cfg(target_os = "macos")]
fn run_window(cfg: WorkerConfig, keys: KeyOptions, hotkey: ReleaseHotkey) -> anyhow::Result<()> {
    mac::run(cfg, keys, hotkey)
}

#[cfg(not(target_os = "macos"))]
fn run_window(_: WorkerConfig, _: KeyOptions, _: ReleaseHotkey) -> anyhow::Result<()> {
    anyhow::bail!("the window needs macOS; on this system use --dump DIR, or the GTK `gliff`")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("gliff-mac").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn cli_builds_the_server_arguments() {
        let Endpoint::Ssh(t) = parse(&["me@box"]).endpoint().unwrap() else {
            panic!()
        };
        assert_eq!(t.server_args, ["--output", "auto"]);
        let Endpoint::Ssh(t) = parse(&["--headless", "me@box"]).endpoint().unwrap() else {
            panic!()
        };
        assert_eq!(t.server_args, ["--headless"]);
        let Endpoint::Ssh(t) = parse(&["--output", "DP-1", "me@box"]).endpoint().unwrap() else {
            panic!()
        };
        assert_eq!(t.server_args, ["--output", "DP-1"]);
        assert!(matches!(
            parse(&["--connect", "127.0.0.1:7000"]).endpoint().unwrap(),
            Endpoint::Tcp(_)
        ));
    }

    #[test]
    fn hostile_hosts_are_refused() {
        assert!(validate_host("-oProxyCommand=x").is_err());
        assert!(validate_host("a b").is_err());
        assert!(validate_host("me@box").is_ok());
    }

    #[test]
    fn keymaps_are_checked() {
        let dir = std::env::temp_dir().join(format!("gliff-mac-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.xkb");
        std::fs::write(&good, "xkb_keymap { };").unwrap();
        assert_eq!(read_keymap(Some(&good)).unwrap(), "xkb_keymap { };");
        let bad = dir.join("bad.txt");
        std::fs::write(&bad, "hello").unwrap();
        assert!(read_keymap(Some(&bad)).is_err());
        assert_eq!(read_keymap(None).unwrap(), "");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn options_parse() {
        let cli = parse(&["--command-as", "ctrl", "--chroma", "reduced", "h"]);
        assert_eq!(cli.command_as, CommandAs::Ctrl);
        assert_eq!(cli.chroma, ChromaPref::Reduced);
    }
}
