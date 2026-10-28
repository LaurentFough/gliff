//! The machines this client has connected to, in the order they were first
//! connected, kept in `$XDG_CONFIG_HOME/gliff/config.toml` so the header can
//! offer them again as tabs. It also keeps which of them were connected and
//! which was shown, so a reopened window connects to them again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::settings::Settings;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default, alias = "recent_machines")]
    pub machines: Vec<String>,
    /// The machines with a running session, in tab order of opening.
    #[serde(default)]
    pub open: Vec<String>,
    /// The machine the window showed last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown: Option<String>,
    /// The options for every machine, where they differ from the built-in
    /// values.
    #[serde(default, skip_serializing_if = "Settings::is_empty")]
    pub defaults: Settings,
    /// What one machine changes from the defaults.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub overrides: std::collections::BTreeMap<String, Settings>,
}

impl Config {
    /// Where the client keeps its config: `~/.config/gliff/config.toml`.
    pub fn default_path() -> PathBuf {
        gtk4::glib::user_config_dir()
            .join("gliff")
            .join("config.toml")
    }

    /// Read the config, or start empty when there is none yet. A file that
    /// cannot be parsed is logged and treated as empty, so a hand edit gone
    /// wrong never stops the client from starting.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read config");
                return Self::default();
            }
        };
        match Self::parse(&text) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot parse config");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let mut config: Self = toml::from_str(text)?;
        config.defaults.tidy();
        for o in config.overrides.values_mut() {
            o.tidy();
        }
        config.overrides.retain(|_, o| !o.is_empty());
        config.machines.retain(|m| !m.trim().is_empty());
        let mut seen = std::collections::HashSet::new();
        config.machines.retain(|m| seen.insert(m.clone()));
        // Only remembered machines can be reopened.
        let machines = config.machines.clone();
        config.open.retain(|m| machines.contains(m));
        let mut seen = std::collections::HashSet::new();
        config.open.retain(|m| seen.insert(m.clone()));
        Ok(config)
    }

    /// Add `machine` at the end of the list unless it is already there, so
    /// the tabs keep their places as machines are reconnected.
    pub fn remember(&mut self, machine: &str) {
        let machine = machine.trim();
        if machine.is_empty() || self.machines.iter().any(|m| m == machine) {
            return;
        }
        self.machines.push(machine.to_string());
    }

    /// What `machine` changes from the defaults; nothing when it has no
    /// override.
    pub fn machine(&self, machine: &str) -> Option<&Settings> {
        self.overrides.get(machine)
    }

    /// Replace a machine's override, dropping the entry when it changes
    /// nothing.
    pub fn set_machine(&mut self, machine: &str, mut settings: Settings) {
        settings.tidy();
        if settings.is_empty() {
            self.overrides.remove(machine);
        } else {
            self.overrides.insert(machine.to_string(), settings);
        }
    }

    /// The options `machine` runs with, with `cli` standing in for the
    /// defaults the command line gives.
    pub fn settings_for(&self, cli: &Settings, machine: &str) -> Settings {
        let base = cli.over(&self.defaults);
        match self.overrides.get(machine) {
            Some(o) => o.over(&base),
            None => base,
        }
    }

    pub fn forget(&mut self, machine: &str) {
        self.overrides.remove(machine);
        self.machines.retain(|m| m != machine);
        self.set_open(machine, false);
        if self.shown.as_deref() == Some(machine) {
            self.shown = None;
        }
    }

    /// Record whether `machine` has a running session, to reconnect it when
    /// the window opens again.
    pub fn set_open(&mut self, machine: &str, open: bool) {
        let listed = self.open.iter().any(|m| m == machine);
        if open && !listed {
            self.open.push(machine.to_string());
        } else if !open {
            self.open.retain(|m| m != machine);
        }
    }

    /// Move `machine` to just before `target`, or just after it with
    /// `after`. False when either is not in the list, or they are the same.
    pub fn move_machine(&mut self, machine: &str, target: &str, after: bool) -> bool {
        if machine == target || !self.machines.iter().any(|m| m == target) {
            return false;
        }
        let Some(from) = self.machines.iter().position(|m| m == machine) else {
            return false;
        };
        let moved = self.machines.remove(from);
        let to = self.machines.iter().position(|m| m == target).unwrap() + usize::from(after);
        self.machines.insert(to, moved);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Field, Screen};

    #[test]
    fn remember_appends_new_machines_once() {
        let mut c = Config::default();
        c.remember("a");
        c.remember("b");
        c.remember("a");
        assert_eq!(c.machines, vec!["a", "b"]);
    }

    #[test]
    fn remember_trims_and_ignores_blank() {
        let mut c = Config::default();
        c.remember("  ");
        c.remember(" user@host ");
        assert_eq!(c.machines, vec!["user@host"]);
    }

    #[test]
    fn forget_removes_the_machine() {
        let mut c = Config::default();
        c.remember("a");
        c.remember("b");
        c.forget("a");
        c.forget("missing");
        assert_eq!(c.machines, vec!["b"]);
    }

    #[test]
    fn settings_layer_defaults_then_the_command_line_then_the_machine() {
        let mut c = Config::default();
        c.defaults.screen = Some(Screen::Own);
        c.defaults.clipboard = Some(false);
        let cli = Settings {
            screen: Some(Screen::Mirror),
            ..Settings::default()
        };
        assert_eq!(c.settings_for(&cli, "a").resolve().screen, Screen::Mirror);
        c.set_machine(
            "a",
            Settings {
                screen: Some(Screen::Own),
                ..Settings::default()
            },
        );
        let r = c.settings_for(&cli, "a").resolve();
        assert_eq!(r.screen, Screen::Own);
        assert!(!r.clipboard);
        let mut o = c.machine("a").cloned().unwrap();
        o.clear(Field::Screen);
        c.set_machine("a", o);
        assert!(c.machine("a").is_none());
        assert_eq!(c.settings_for(&cli, "a").resolve().screen, Screen::Mirror);
    }

    #[test]
    fn overrides_and_defaults_round_trip_and_go_with_the_machine() {
        let mut c = Config::default();
        c.remember("a");
        c.defaults.clipboard = Some(false);
        c.set_machine(
            "a",
            Settings {
                server_bin: Some("env X=1 gliff-server".into()),
                ..Settings::default()
            },
        );
        assert_eq!(Config::parse(&toml::to_string(&c).unwrap()).unwrap(), c);
        c.forget("a");
        assert!(c.overrides.is_empty());
        assert_eq!(c.defaults.clipboard, Some(false));
    }

    #[test]
    fn set_open_records_each_machine_once() {
        let mut c = Config::default();
        c.set_open("a", true);
        c.set_open("b", true);
        c.set_open("a", true);
        assert_eq!(c.open, vec!["a", "b"]);
        c.set_open("a", false);
        assert_eq!(c.open, vec!["b"]);
    }

    #[test]
    fn forget_closes_and_unshows_the_machine() {
        let mut c = Config::default();
        c.remember("a");
        c.set_open("a", true);
        c.shown = Some("a".into());
        c.forget("a");
        assert!(c.machines.is_empty() && c.open.is_empty());
        assert_eq!(c.shown, None);
    }

    #[test]
    fn parse_keeps_only_remembered_open_machines() {
        let c =
            Config::parse("machines = [\"a\", \"b\"]\nopen = [\"b\", \"x\", \"b\"]\nshown = \"b\"")
                .unwrap();
        assert_eq!(c.open, vec!["b"]);
        assert_eq!(c.shown.as_deref(), Some("b"));
    }

    #[test]
    fn move_machine_places_before_or_after_the_target() {
        let mut c = Config::default();
        for m in ["a", "b", "c", "d"] {
            c.remember(m);
        }
        assert!(c.move_machine("d", "b", false));
        assert_eq!(c.machines, vec!["a", "d", "b", "c"]);
        assert!(c.move_machine("a", "c", true));
        assert_eq!(c.machines, vec!["d", "b", "c", "a"]);
        assert!(c.move_machine("a", "d", false));
        assert_eq!(c.machines, vec!["a", "d", "b", "c"]);
    }

    #[test]
    fn move_machine_ignores_unknown_and_self() {
        let mut c = Config::default();
        c.remember("a");
        c.remember("b");
        assert!(!c.move_machine("a", "a", true));
        assert!(!c.move_machine("x", "a", false));
        assert!(!c.move_machine("a", "x", false));
        assert_eq!(c.machines, vec!["a", "b"]);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config::default();
        c.remember("b");
        c.remember("a");
        c.set_open("a", true);
        c.shown = Some("a".into());
        let text = toml::to_string(&c).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), c);
    }

    #[test]
    fn parse_tolerates_missing_key_and_drops_blanks_and_duplicates() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let c = Config::parse("machines = [\"a\", \"\", \"b\", \"a\"]").unwrap();
        assert_eq!(c.machines, vec!["a", "b"]);
        assert!(Config::parse("machines = 3").is_err());
    }

    #[test]
    fn parse_reads_the_old_recent_machines_key() {
        let c = Config::parse("recent_machines = [\"a\", \"b\"]").unwrap();
        assert_eq!(c.machines, vec!["a", "b"]);
    }

    #[test]
    fn load_and_save_files() {
        let dir = std::env::temp_dir().join(format!("gliff-recent-{}", std::process::id()));
        let path = dir.join("nested").join("config.toml");
        assert_eq!(Config::load(&path), Config::default());

        let mut c = Config::default();
        c.remember("user@host");
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path), c);

        std::fs::write(&path, "not toml [").unwrap();
        assert_eq!(Config::load(&path), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
