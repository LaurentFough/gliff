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
}

/// An option, to ask where its value comes from or to clear it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Screen,
    Clipboard,
    Decode,
    ServerBin,
}

impl Field {
    pub const ALL: [Field; 4] = [
        Field::Screen,
        Field::Clipboard,
        Field::Decode,
        Field::ServerBin,
    ];
}

impl Settings {
    pub fn is_set(&self, field: Field) -> bool {
        match field {
            Field::Screen => self.screen.is_some(),
            Field::Clipboard => self.clipboard.is_some(),
            Field::Decode => self.decode.is_some(),
            Field::ServerBin => self.server_bin.is_some(),
        }
    }

    pub fn clear(&mut self, field: Field) {
        match field {
            Field::Screen => self.screen = None,
            Field::Clipboard => self.clipboard = None,
            Field::Decode => self.decode = None,
            Field::ServerBin => self.server_bin = None,
        }
    }

    /// Whether the table changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Normalise what a hand edit or a text box can leave: a blank server
    /// command is no command.
    pub fn tidy(&mut self) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_set_resolves_to_the_built_in_values() {
        let r = Settings::default().resolve();
        assert_eq!(r.screen, Screen::Mirror);
        assert!(r.clipboard);
        assert_eq!(r.decode, Video::Auto);
        assert_eq!(r.server_bin, "gliff-server");
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
