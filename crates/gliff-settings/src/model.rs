//! The per-session options and how they layer. Every option has a built-in
//! value; the config's `[defaults]` table can change it for all machines and
//! an `[overrides."user@host"]` table for one machine. A table holds only
//! what it changes, so a machine with no override follows the defaults, and
//! clearing an override hands the option back to them.
//!
//! The order, last wins: built-in, `[defaults]`, the command line, the
//! machine's override. The command line sits below the override because a
//! choice made in the window has always beaten the flags.

use serde::{Deserialize, Serialize};

/// How the machine is shown.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Screen {
    /// The machine's focused screen, fitted into the window.
    #[default]
    Mirror,
    /// A private screen of its own, sized and scaled to the window.
    Own,
}

/// Which video pipeline decodes here.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Video {
    /// The GPU when it works, else the CPU.
    #[default]
    Auto,
    Gpu,
    Cpu,
}

impl Video {
    /// The value `--video` takes, none for automatic.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Gpu => Some("gpu"),
            Self::Cpu => Some("cpu"),
        }
    }
}

pub const DEFAULT_SERVER_BIN: &str = "gliff-server";

/// The options one table changes. `None` leaves an option to the layer
/// below.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// How the machine is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<Screen>,
    /// Whether the clipboard is shared with the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<bool>,
    /// The video pipeline that decodes here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<Video>,
    /// The command that starts the server over ssh; blank is `gliff-server`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_bin: Option<String>,
    /// Reconnect without being asked: reopen the machine's tab at launch when
    /// it was open at quit, and retry a session that dropped. Off, the client
    /// waits for the user (default on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_reconnect: Option<bool>,
    /// Mute the machine's own speakers while this client plays its sound
    /// (the server's `--mute-local-audio`; Linux hosts with PipeWire).
    /// Default off: the speakers keep playing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute_host_speakers: Option<bool>,
    /// The SSH login for an address without `user@` (a mesh peer, a bare
    /// host name): the client connects as `ssh_user@host`. Unset, ssh
    /// decides (`~/.ssh/config`, else the local user name). An address that
    /// names a user keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_user: Option<String>,
}

/// Whether `user` is a login name Gliff passes to ssh: 1 to 64 of
/// `A-Z a-z 0-9 _ . -`, not starting with `.` or `-` (no option or `@`
/// can sneak into the address).
pub fn valid_ssh_user(user: &str) -> bool {
    let mut chars = user.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric() || c == '_')
        && user.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// An option, to ask where its value comes from or to clear it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Screen,
    Clipboard,
    Decode,
    ServerBin,
    AutoReconnect,
    MuteHostSpeakers,
    SshUser,
}

impl Field {
    pub const ALL: [Field; 7] = [
        Field::Screen,
        Field::Clipboard,
        Field::Decode,
        Field::ServerBin,
        Field::AutoReconnect,
        Field::MuteHostSpeakers,
        Field::SshUser,
    ];
}

impl Settings {
    pub fn is_set(&self, field: Field) -> bool {
        match field {
            Field::Screen => self.screen.is_some(),
            Field::Clipboard => self.clipboard.is_some(),
            Field::Decode => self.decode.is_some(),
            Field::ServerBin => self.server_bin.is_some(),
            Field::AutoReconnect => self.auto_reconnect.is_some(),
            Field::MuteHostSpeakers => self.mute_host_speakers.is_some(),
            Field::SshUser => self.ssh_user.is_some(),
        }
    }

    pub fn clear(&mut self, field: Field) {
        match field {
            Field::Screen => self.screen = None,
            Field::Clipboard => self.clipboard = None,
            Field::Decode => self.decode = None,
            Field::ServerBin => self.server_bin = None,
            Field::AutoReconnect => self.auto_reconnect = None,
            Field::MuteHostSpeakers => self.mute_host_speakers = None,
            Field::SshUser => self.ssh_user = None,
        }
    }

    /// Whether the table changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Normalise what a hand edit or a text box can leave: a blank server
    /// command is no command; a blank SSH user is none (surrounding spaces
    /// trimmed).
    pub fn tidy(&mut self) {
        if let Some(user) = &mut self.ssh_user {
            *user = user.trim().to_string();
            if user.is_empty() {
                self.ssh_user = None;
            }
        }
        if self
            .server_bin
            .as_deref()
            .is_some_and(|s| s.trim().is_empty())
        {
            self.server_bin = None;
        }
    }

    /// `self` on top of `below`: what `self` sets, else what `below` does.
    pub fn over(&self, below: &Settings) -> Settings {
        Settings {
            screen: self.screen.or(below.screen),
            clipboard: self.clipboard.or(below.clipboard),
            decode: self.decode.or(below.decode),
            server_bin: self.server_bin.clone().or_else(|| below.server_bin.clone()),
            auto_reconnect: self.auto_reconnect.or(below.auto_reconnect),
            mute_host_speakers: self.mute_host_speakers.or(below.mute_host_speakers),
            ssh_user: self.ssh_user.clone().or_else(|| below.ssh_user.clone()),
        }
    }

    /// Every option with its value: what a session runs with once the
    /// layers are stacked.
    pub fn resolve(&self) -> Resolved {
        Resolved {
            screen: self.screen.unwrap_or_default(),
            clipboard: self.clipboard.unwrap_or(true),
            decode: self.decode.unwrap_or_default(),
            server_bin: self
                .server_bin
                .clone()
                .unwrap_or_else(|| DEFAULT_SERVER_BIN.to_string()),
            auto_reconnect: self.auto_reconnect.unwrap_or(true),
            mute_host_speakers: self.mute_host_speakers.unwrap_or(false),
            //= A hand-edited name ssh could misread is ignored, not passed on.
            ssh_user: self
                .ssh_user
                .as_deref()
                .map(str::trim)
                .filter(|u| valid_ssh_user(u))
                .map(str::to_string),
        }
    }
}

/// A session's options with every value filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub screen: Screen,
    pub clipboard: bool,
    pub decode: Video,
    pub server_bin: String,
    pub auto_reconnect: bool,
    pub mute_host_speakers: bool,
    /// The login for an address without `user@`; none leaves it to ssh.
    pub ssh_user: Option<String>,
}

impl Resolved {
    /// The address ssh connects to: `host` as given when it names a user
    /// (or no SSH user is set), else `ssh_user@host`.
    pub fn ssh_address(&self, host: &str) -> String {
        match &self.ssh_user {
            Some(user) if !host.contains('@') && !host.is_empty() => format!("{user}@{host}"),
            _ => host.to_string(),
        }
    }

    /// Server arguments these options add (the screen is handled by each
    /// client, as `--output` can override it).
    pub fn server_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        if self.mute_host_speakers {
            flags.push("--mute-local-audio".to_string());
        }
        flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ssh_user_fills_in_only_a_bare_address() {
        let mut s = Settings {
            ssh_user: Some(" el ".into()),
            ..Settings::default()
        };
        s.tidy();
        assert_eq!(s.ssh_user.as_deref(), Some("el"));
        let r = s.resolve();
        assert_eq!(
            r.ssh_address("studio.tail1234.ts.net"),
            "el@studio.tail1234.ts.net"
        );
        assert_eq!(
            r.ssh_address("me@box"),
            "me@box",
            "an address that names a user keeps it"
        );
        assert_eq!(Settings::default().resolve().ssh_address("box"), "box");
        let below = Settings {
            ssh_user: Some("defaults".into()),
            ..Settings::default()
        };
        assert_eq!(s.over(&below).ssh_user.as_deref(), Some("el"));
        assert_eq!(
            Settings::default()
                .over(&below)
                .resolve()
                .ssh_user
                .as_deref(),
            Some("defaults")
        );
        let mut blank = Settings {
            ssh_user: Some("  ".into()),
            ..Settings::default()
        };
        blank.tidy();
        assert!(blank.ssh_user.is_none());
    }

    #[test]
    fn ssh_users_that_could_become_options_are_refused() {
        for good in ["el", "_svc", "a.b-c", "x1"] {
            assert!(valid_ssh_user(good), "{good}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            ".hidden",
            "a b",
            "me@x",
            "a;b",
            &"a".repeat(65),
        ] {
            assert!(!valid_ssh_user(bad), "{bad}");
            let s = Settings {
                ssh_user: Some(bad.to_string()),
                ..Settings::default()
            };
            assert!(s.resolve().ssh_user.is_none(), "{bad}");
        }
    }

    #[test]
    fn nothing_set_resolves_to_the_built_in_values() {
        let r = Settings::default().resolve();
        assert_eq!(r.screen, Screen::Mirror);
        assert!(r.clipboard);
        assert_eq!(r.decode, Video::Auto);
        assert_eq!(r.server_bin, "gliff-server");
        assert!(
            r.auto_reconnect,
            "reconnecting is on unless a machine turns it off"
        );
        assert!(!r.mute_host_speakers, "the host's speakers keep playing");
        assert!(r.server_flags().is_empty());
    }

    #[test]
    fn muting_the_host_speakers_adds_the_server_flag() {
        let defaults = Settings {
            mute_host_speakers: Some(true),
            ..Settings::default()
        };
        let r = Settings::default().over(&defaults).resolve();
        assert_eq!(r.server_flags(), vec!["--mute-local-audio".to_string()]);
        let machine = Settings {
            mute_host_speakers: Some(false),
            ..Settings::default()
        };
        assert!(machine.over(&defaults).resolve().server_flags().is_empty());
    }

    #[test]
    fn auto_reconnect_layers_like_the_other_options() {
        let defaults = Settings {
            auto_reconnect: Some(false),
            ..Settings::default()
        };
        let machine = Settings {
            auto_reconnect: Some(true),
            ..Settings::default()
        };
        assert!(!Settings::default().over(&defaults).resolve().auto_reconnect);
        assert!(machine.over(&defaults).resolve().auto_reconnect);
        let mut m = machine;
        assert!(m.is_set(Field::AutoReconnect));
        m.clear(Field::AutoReconnect);
        assert!(m.is_empty());
    }

    #[test]
    fn a_machine_overrides_the_defaults_only_where_it_sets() {
        let defaults = Settings {
            screen: Some(Screen::Own),
            clipboard: Some(false),
            ..Settings::default()
        };
        let machine = Settings {
            clipboard: Some(true),
            ..Settings::default()
        };
        let r = machine.over(&defaults).resolve();
        assert_eq!(r.screen, Screen::Own);
        assert!(r.clipboard);
    }

    #[test]
    fn clearing_an_override_hands_the_option_back() {
        let defaults = Settings {
            clipboard: Some(false),
            ..Settings::default()
        };
        let mut machine = Settings {
            clipboard: Some(true),
            ..Settings::default()
        };
        assert!(machine.is_set(Field::Clipboard));
        machine.clear(Field::Clipboard);
        assert!(machine.is_empty());
        assert!(!machine.over(&defaults).resolve().clipboard);
    }

    #[test]
    fn a_blank_server_command_is_no_command() {
        let mut s = Settings {
            server_bin: Some("  ".into()),
            ..Settings::default()
        };
        s.tidy();
        assert_eq!(s.server_bin, None);
    }

    #[test]
    fn only_what_is_set_is_written() {
        let s = Settings {
            clipboard: Some(false),
            ..Settings::default()
        };
        assert_eq!(toml::to_string(&s).unwrap().trim(), "clipboard = false");
    }
}
