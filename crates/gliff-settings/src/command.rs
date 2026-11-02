//! The client command line a machine's settings come to, for Settings to
//! show and copy: start the same connection from a terminal, with logs.
//!
//! Every option that has a flag is spelled out, so the line also works
//! where the config is different (or absent); `auto_reconnect` has none, as
//! one terminal run does not reopen tabs. A machine's own settings still
//! sit above the flags when it has them, so the line never changes what a
//! configured machine does.
//!
//! Version: 1.0.0

use crate::{Resolved, Screen, DEFAULT_SERVER_BIN};

/// `arg` for a POSIX shell, quoted only when it needs to be.
pub fn shell_word(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// The client's flags for these options (no program, no host).
pub fn client_flags(r: &Resolved) -> Vec<String> {
    let mut flags = Vec::new();
    if r.screen == Screen::Own {
        flags.push("--headless".to_string());
    }
    if !r.clipboard {
        flags.push("--no-clipboard".to_string());
    }
    if let Some(video) = r.decode.flag() {
        flags.extend(["--video".to_string(), video.to_string()]);
    }
    if r.server_bin.trim() != DEFAULT_SERVER_BIN {
        flags.extend(["--server-bin".to_string(), r.server_bin.clone()]);
    }
    if r.mute_host_speakers {
        flags.push("--mute-host-speakers".to_string());
    }
    flags
}

/// `RUST_LOG=info PROGRAM [--port N] FLAGS… HOST`, quoted for a shell.
/// `RUST_LOG` makes the client log the connection to the terminal.
pub fn client_line(program: &str, host: &str, port: Option<u16>, r: &Resolved) -> String {
    let mut words = vec!["RUST_LOG=info".to_string(), shell_word(program)];
    if let Some(port) = port.filter(|p| *p != 22) {
        words.extend(["--port".to_string(), port.to_string()]);
    }
    words.extend(client_flags(r).iter().map(|w| shell_word(w)));
    words.push(shell_word(host));
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Settings, Video};

    #[test]
    fn built_in_settings_need_no_flags() {
        let r = Settings::default().resolve();
        assert!(client_flags(&r).is_empty());
        assert_eq!(
            client_line("gliff", "me@box", Some(22), &r),
            "RUST_LOG=info gliff me@box"
        );
    }

    #[test]
    fn every_option_with_a_flag_is_spelled_out() {
        let r = Settings {
            screen: Some(Screen::Own),
            clipboard: Some(false),
            decode: Some(Video::Cpu),
            server_bin: Some("env X=1 gliff-server".into()),
            auto_reconnect: Some(false),
            mute_host_speakers: Some(true),
        }
        .resolve();
        assert_eq!(
            client_line("gliff", "me@box", Some(2222), &r),
            "RUST_LOG=info gliff --port 2222 --headless --no-clipboard --video cpu \
             --server-bin 'env X=1 gliff-server' --mute-host-speakers me@box"
        );
    }

    #[test]
    fn words_are_quoted_only_when_needed() {
        assert_eq!(shell_word("user@host.local"), "user@host.local");
        assert_eq!(shell_word("a b"), "'a b'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word(""), "''");
    }
}
