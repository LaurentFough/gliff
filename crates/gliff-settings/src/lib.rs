//! Per-machine client options, shared by the Linux (GTK) and macOS clients.
//!
//! - [`model`]: the options, how the layers stack, and what a session runs
//!   with ([`Settings`], [`Resolved`]). See `docs/settings.md`.
//! - [`store`]: reading and writing `~/.config/gliff/config.toml` without
//!   disturbing the keys a client does not own.
//!
//! Both clients read the same file format, so a `[defaults]` or
//! `[overrides."user@host"]` table means the same thing on either.

#![forbid(unsafe_code)]

pub mod model;
pub mod store;

pub use model::*;
pub use store::Store;

impl Field {
    /// The option's key in the config file and in the macOS UI protocol.
    pub fn name(self) -> &'static str {
        match self {
            Field::Screen => "screen",
            Field::Clipboard => "clipboard",
            Field::Decode => "decode",
            Field::ServerBin => "server_bin",
            Field::AutoReconnect => "auto_reconnect",
        }
    }

    /// The option with this key, if any.
    pub fn from_name(name: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_round_trip_and_match_the_config_keys() {
        for field in Field::ALL {
            assert_eq!(Field::from_name(field.name()), Some(field));
        }
        assert_eq!(Field::from_name("nope"), None);
        //= The names are the serde keys, so a UI edit and a file edit agree.
        let s = Settings {
            screen: Some(Screen::Own),
            clipboard: Some(false),
            decode: Some(Video::Cpu),
            server_bin: Some("x".into()),
            auto_reconnect: Some(false),
        };
        let text = toml::to_string(&s).unwrap();
        for field in Field::ALL {
            assert!(text.contains(&format!("{} =", field.name())), "{text}");
        }
    }
}
