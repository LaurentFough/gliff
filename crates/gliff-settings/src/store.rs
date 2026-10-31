//! The config file both clients share: `$XDG_CONFIG_HOME/gliff/config.toml`,
//! else `~/.config/gliff/config.toml`.
//!
//! [`Store`] owns six keys: `machines` (the machines this client has
//! connected to, oldest first), `[defaults]`, `[overrides."user@host"]`,
//! `muted` (the sound switch, shared with the Linux client's header button),
//! `[[open_tabs]]` (the Mac's tabs to reopen at launch, `host` + `port`;
//! the Linux client keeps its own `open` list) and `quit_on_last_window`
//! (the Mac app quits when its last window closes; absent means off).
//! Everything else in the file (the Linux client's `open` and `shown`, or
//! keys from a newer build) is read and written back untouched, so either
//! client can save without losing the other's state.
//!
//! A file that cannot be read or parsed is logged and treated as empty: a
//! hand edit gone wrong never stops a client from starting. Saving replaces
//! the file atomically (write a sibling, then rename).

use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::{Resolved, Settings};

const MACHINES: &str = "machines";
/// An older name for `machines`; renamed on load so the two never coexist
/// (the Linux client reads it as an alias and would reject both).
const LEGACY_MACHINES: &str = "recent_machines";
const DEFAULTS: &str = "defaults";
const OVERRIDES: &str = "overrides";
/// Top-level, as the Linux client writes it: present and true only when
/// sound is off.
const MUTED: &str = "muted";
const OPEN_TABS: &str = "open_tabs";
/// Top-level, Mac only: present and true only when on.
const QUIT_ON_LAST_WINDOW: &str = "quit_on_last_window";

/// The parsed config file and where it lives.
#[derive(Debug, Clone)]
pub struct Store {
    /// `None`: no home directory, so nothing is read or saved.
    path: Option<PathBuf>,
    doc: Table,
}

impl Store {
    /// `$XDG_CONFIG_HOME/gliff/config.toml`, else `$HOME/.config/gliff/config.toml`,
    /// the same path the Linux client uses. `None` without either variable.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".config"))
            })?;
        Some(base.join("gliff").join("config.toml"))
    }

    /// Read the config at the default path (see [`Store::default_path`]).
    pub fn open() -> Self {
        match Self::default_path() {
            Some(path) => Self::load(&path),
            None => {
                tracing::warn!("no HOME or XDG_CONFIG_HOME; settings are not saved");
                Self::in_memory()
            }
        }
    }

    /// A store that is never written, for tests and integration runs.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            doc: Table::new(),
        }
    }

    /// Read `path`, or start empty when it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        let doc = match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "cannot parse config; starting empty");
                Table::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Table::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read config; starting empty");
                Table::new()
            }
        };
        Self {
            path: Some(path.to_path_buf()),
            doc,
        }
    }

    /// Parse config text. Errors only on invalid TOML; a malformed known
    /// table is dropped (and logged) rather than failing the whole file.
    pub fn parse(text: &str) -> Result<Table, toml::de::Error> {
        let mut doc: Table = toml::from_str(text)?;
        if !doc.contains_key(MACHINES) {
            if let Some(v) = doc.remove(LEGACY_MACHINES) {
                doc.insert(MACHINES.into(), v);
            }
        } else {
            doc.remove(LEGACY_MACHINES);
        }
        Ok(doc)
    }

    /// Where the file is saved, when it is.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The `[defaults]` layer.
    pub fn defaults(&self) -> Settings {
        self.doc
            .get(DEFAULTS)
            .and_then(|v| settings_from(v, DEFAULTS))
            .unwrap_or_default()
    }

    /// One machine's override layer (empty when it has none).
    pub fn machine(&self, name: &str) -> Settings {
        self.overrides()
            .and_then(|t| t.get(name))
            .and_then(|v| settings_from(v, name))
            .unwrap_or_default()
    }

    /// Every known machine: the remembered list in order, then machines
    /// that have an override but were never remembered.
    pub fn machines(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(Value::Array(list)) = self.doc.get(MACHINES) {
            for name in list.iter().filter_map(Value::as_str) {
                let name = name.trim();
                if !name.is_empty() && !out.iter().any(|m| m == name) {
                    out.push(name.to_string());
                }
            }
        }
        if let Some(t) = self.overrides() {
            for name in t.keys() {
                if !out.iter().any(|m| m == name) {
                    out.push(name.clone());
                }
            }
        }
        out
    }

    /// Add `name` to the remembered machines. False when it was known.
    pub fn remember(&mut self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let list = match self.doc.get_mut(MACHINES) {
            Some(Value::Array(list)) => list,
            _ => {
                self.doc.insert(MACHINES.into(), Value::Array(Vec::new()));
                match self.doc.get_mut(MACHINES) {
                    Some(Value::Array(list)) => list,
                    _ => unreachable!("just inserted"),
                }
            }
        };
        if list.iter().any(|v| v.as_str() == Some(name)) {
            return false;
        }
        list.push(Value::String(name.to_string()));
        true
    }

    /// Forget a machine, as the Linux client's "Forget" does: its place in
    /// `machines`, its `[overrides]` entry and its tabs to reopen. False
    /// when it was not known.
    pub fn forget(&mut self, name: &str) -> bool {
        let name = name.trim();
        let mut known = false;
        if let Some(Value::Array(list)) = self.doc.get_mut(MACHINES) {
            let before = list.len();
            list.retain(|v| v.as_str().map(str::trim) != Some(name));
            known |= list.len() != before;
        }
        if let Some(Value::Table(t)) = self.doc.get_mut(OVERRIDES) {
            known |= t.remove(name).is_some();
            if t.is_empty() {
                self.doc.remove(OVERRIDES);
            }
        }
        let tabs = self.open_tabs();
        let kept: Vec<_> = tabs.iter().filter(|(h, _)| h != name).cloned().collect();
        if kept.len() != tabs.len() {
            known = true;
            self.set_open_tabs(&kept);
        }
        known
    }

    /// Replace the `[defaults]` layer; an empty layer removes the table.
    pub fn set_defaults(&mut self, mut settings: Settings) {
        settings.tidy();
        match settings_to(&settings) {
            Some(t) => {
                self.doc.insert(DEFAULTS.into(), Value::Table(t));
            }
            None => {
                self.doc.remove(DEFAULTS);
            }
        }
    }

    /// Replace one machine's override; an empty one is removed, and so is
    /// `[overrides]` when nothing is left in it.
    pub fn set_machine(&mut self, name: &str, mut settings: Settings) {
        settings.tidy();
        let entry = settings_to(&settings);
        let overrides = match self.doc.get_mut(OVERRIDES) {
            Some(Value::Table(t)) => t,
            _ => {
                if entry.is_none() {
                    return;
                }
                self.doc
                    .insert(OVERRIDES.into(), Value::Table(Table::new()));
                match self.doc.get_mut(OVERRIDES) {
                    Some(Value::Table(t)) => t,
                    _ => unreachable!("just inserted"),
                }
            }
        };
        match entry {
            Some(t) => {
                overrides.insert(name.to_string(), Value::Table(t));
            }
            None => {
                overrides.remove(name);
            }
        }
        if overrides.is_empty() {
            self.doc.remove(OVERRIDES);
        }
    }

    /// Sound is switched off for every machine (`muted = true`).
    pub fn muted(&self) -> bool {
        self.doc
            .get(MUTED)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Switch sound off or on; "on" removes the key, as the Linux client does.
    pub fn set_muted(&mut self, muted: bool) {
        if muted {
            self.doc.insert(MUTED.into(), Value::Boolean(true));
        } else {
            self.doc.remove(MUTED);
        }
    }

    /// The Mac app quits when its last window closes
    /// (`quit_on_last_window = true`); off by default.
    pub fn quit_on_last_window(&self) -> bool {
        self.doc
            .get(QUIT_ON_LAST_WINDOW)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Switch it on or off; off removes the key.
    pub fn set_quit_on_last_window(&mut self, on: bool) {
        if on {
            self.doc
                .insert(QUIT_ON_LAST_WINDOW.into(), Value::Boolean(true));
        } else {
            self.doc.remove(QUIT_ON_LAST_WINDOW);
        }
    }

    /// The Mac's tabs to reopen, in order: (SSH address, port). Malformed
    /// entries are skipped.
    pub fn open_tabs(&self) -> Vec<(String, u16)> {
        let Some(Value::Array(list)) = self.doc.get(OPEN_TABS) else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|v| {
                let t = v.as_table()?;
                let host = t.get("host")?.as_str()?.trim();
                let port = t.get("port").and_then(Value::as_integer).unwrap_or(22);
                let port = u16::try_from(port).ok().filter(|p| *p > 0)?;
                (!host.is_empty()).then(|| (host.to_string(), port))
            })
            .collect()
    }

    /// Replace the tabs to reopen; none removes the key.
    pub fn set_open_tabs(&mut self, tabs: &[(String, u16)]) {
        if tabs.is_empty() {
            self.doc.remove(OPEN_TABS);
            return;
        }
        let list = tabs
            .iter()
            .map(|(host, port)| {
                let mut t = Table::new();
                t.insert("host".into(), Value::String(host.clone()));
                t.insert("port".into(), Value::Integer(i64::from(*port)));
                Value::Table(t)
            })
            .collect();
        self.doc.insert(OPEN_TABS.into(), Value::Array(list));
    }

    /// What a session with `name` runs with, given the command line's layer:
    /// built-in, then `[defaults]`, then `cli`, then the machine's override.
    pub fn resolve(&self, name: &str, cli: &Settings) -> Resolved {
        self.machine(name)
            .over(&cli.over(&self.defaults()))
            .resolve()
    }

    /// Write the file, creating its directory. A no-op for an in-memory store.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(&self.doc).map_err(std::io::Error::other)?;
        let tmp = path.with_extension(format!("toml.tmp-{}", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    fn overrides(&self) -> Option<&Table> {
        self.doc.get(OVERRIDES).and_then(Value::as_table)
    }
}

/// A layer from its table; a malformed one is logged and ignored. Goes
/// through TOML text so it depends only on `to_string`/`from_str`.
fn settings_from(value: &Value, what: &str) -> Option<Settings> {
    let parsed = match value {
        Value::Table(t) => toml::to_string(t)
            .map_err(|e| e.to_string())
            .and_then(|text| toml::from_str::<Settings>(&text).map_err(|e| e.to_string())),
        _ => Err("not a table".to_string()),
    };
    match parsed {
        Ok(mut s) => {
            s.tidy();
            Some(s)
        }
        Err(e) => {
            tracing::warn!(table = what, error = %e, "ignoring a malformed settings table");
            None
        }
    }
}

/// A layer as a table, or `None` when it sets nothing.
fn settings_to(settings: &Settings) -> Option<Table> {
    if settings.is_empty() {
        return None;
    }
    let table = toml::to_string(settings)
        .map_err(|e| e.to_string())
        .and_then(|text| toml::from_str::<Table>(&text).map_err(|e| e.to_string()));
    match table {
        Ok(t) if !t.is_empty() => Some(t),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "cannot encode settings");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Screen, Video};

    fn store(text: &str) -> Store {
        Store {
            path: None,
            doc: Store::parse(text).unwrap(),
        }
    }

    #[test]
    fn keys_it_does_not_own_survive_a_save() {
        let mut s = store("open = [\"a@b\"]\nshown = \"a@b\"\nfuture = 7\nmachines = [\"a@b\"]\n");
        s.set_machine(
            "a@b",
            Settings {
                clipboard: Some(false),
                ..Settings::default()
            },
        );
        let text = toml::to_string(&s.doc).unwrap();
        let back = store(&text);
        assert_eq!(back.doc.get("future").and_then(Value::as_integer), Some(7));
        assert_eq!(back.doc.get("shown").and_then(Value::as_str), Some("a@b"));
        assert_eq!(back.machine("a@b").clipboard, Some(false));
    }

    #[test]
    fn layers_stack_in_the_documented_order() {
        let s = store(
            "[defaults]\nscreen = \"own\"\nclipboard = false\n\
             [overrides.\"m@h\"]\nclipboard = true\n",
        );
        let cli = Settings {
            server_bin: Some("env X=1 gliff-server".into()),
            clipboard: Some(false),
            ..Settings::default()
        };
        let r = s.resolve("m@h", &cli);
        assert_eq!(r.screen, Screen::Own); //= defaults
        assert!(r.clipboard); //= the override beats the command line
        assert_eq!(r.server_bin, "env X=1 gliff-server"); //= command line
        assert_eq!(r.decode, Video::Auto); //= built-in
        assert!(!s.resolve("other@h", &Settings::default()).clipboard);
    }

    #[test]
    fn empty_layers_disappear_from_the_file() {
        let mut s = store("");
        s.set_machine(
            "m@h",
            Settings {
                screen: Some(Screen::Own),
                ..Settings::default()
            },
        );
        assert!(s.doc.contains_key(OVERRIDES));
        s.set_machine("m@h", Settings::default());
        assert!(!s.doc.contains_key(OVERRIDES));
        s.set_defaults(Settings {
            server_bin: Some("   ".into()),
            ..Settings::default()
        });
        assert!(
            !s.doc.contains_key(DEFAULTS),
            "a blank command is no command"
        );
    }

    #[test]
    fn machines_merge_the_list_and_the_overrides() {
        let mut s = store(
            "recent_machines = [\"a@b\", \" \", \"a@b\"]\n[overrides.\"c@d\"]\nclipboard = false\n",
        );
        assert!(!s.doc.contains_key(LEGACY_MACHINES));
        assert_eq!(s.machines(), ["a@b", "c@d"]);
        assert!(s.remember("e@f"));
        assert!(!s.remember("a@b"));
        assert!(!s.remember("  "));
        assert_eq!(s.machines(), ["a@b", "e@f", "c@d"]);
    }

    #[test]
    fn a_malformed_table_is_ignored_not_fatal() {
        let s = store("[defaults]\nscreen = \"sideways\"\n[overrides]\n\"m@h\" = 3\n");
        assert_eq!(s.defaults(), Settings::default());
        assert_eq!(s.machine("m@h"), Settings::default());
        assert!(Store::parse("not toml [").is_err());
    }

    #[test]
    fn quit_on_last_window_is_off_unless_set() {
        let mut s = store("shown = \"a@b\"\n");
        assert!(!s.quit_on_last_window(), "off by default");
        s.set_quit_on_last_window(true);
        assert!(s.quit_on_last_window());
        s.set_quit_on_last_window(false);
        assert!(
            !s.doc.contains_key(QUIT_ON_LAST_WINDOW),
            "off leaves no key"
        );
        assert_eq!(s.doc.get("shown").and_then(Value::as_str), Some("a@b"));
        assert!(!store("quit_on_last_window = 1\n").quit_on_last_window());
    }

    #[test]
    fn muted_is_the_linux_clients_top_level_key() {
        let mut s = store("muted = true\nshown = \"a@b\"\n");
        assert!(s.muted());
        s.set_muted(false);
        assert!(!s.doc.contains_key(MUTED), "sound on leaves no key behind");
        assert_eq!(s.doc.get("shown").and_then(Value::as_str), Some("a@b"));
        s.set_muted(true);
        assert_eq!(
            toml::to_string(&s.doc)
                .unwrap()
                .matches("muted = true")
                .count(),
            1
        );
        assert!(
            !store("muted = \"yes\"\n").muted(),
            "a malformed value means sound on"
        );
    }

    #[test]
    fn open_tabs_round_trip_and_skip_bad_entries() {
        let mut s = store("open = [\"linux@tab\"]\n");
        assert!(s.open_tabs().is_empty());
        s.set_open_tabs(&[("a@b".into(), 22), ("c@d".into(), 2222)]);
        let back = store(&toml::to_string(&s.doc).unwrap());
        assert_eq!(
            back.open_tabs(),
            [("a@b".to_string(), 22), ("c@d".to_string(), 2222)]
        );
        assert_eq!(
            back.doc.get("open").and_then(Value::as_array).map(Vec::len),
            Some(1),
            "Linux's list untouched"
        );
        let bad = store("[[open_tabs]]\nhost = \"\"\n[[open_tabs]]\nhost = \"x@y\"\nport = 70000\n[[open_tabs]]\nhost = \"ok@z\"\n");
        assert_eq!(bad.open_tabs(), [("ok@z".to_string(), 22)]);
        s.set_open_tabs(&[]);
        assert!(!s.doc.contains_key(OPEN_TABS));
    }

    #[test]
    fn forget_removes_a_machine_everywhere_it_is_kept() {
        let mut s = store(
            "machines = [\"a@b\", \"c@d\"]\nopen = [\"a@b\"]\n\
             [overrides.\"a@b\"]\nclipboard = false\n\
             [[open_tabs]]\nhost = \"a@b\"\nport = 22\n\
             [[open_tabs]]\nhost = \"c@d\"\nport = 22\n",
        );
        assert!(s.forget("a@b"));
        assert_eq!(s.machines(), ["c@d"]);
        assert_eq!(s.machine("a@b"), Settings::default());
        assert!(
            !s.doc.contains_key(OVERRIDES),
            "an empty [overrides] goes too"
        );
        assert_eq!(s.open_tabs(), [("c@d".to_string(), 22)]);
        assert!(!s.forget("a@b"), "already forgotten");
        assert_eq!(
            s.doc.get("open").and_then(Value::as_array).map(Vec::len),
            Some(1),
            "Linux's list is its own"
        );
    }

    #[test]
    fn save_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("gliff-settings-{}", std::process::id()));
        let path = dir.join("nested").join("config.toml");
        let mut s = Store::load(&path);
        assert!(s.machines().is_empty());
        s.remember("m@h");
        s.set_defaults(Settings {
            clipboard: Some(false),
            ..Settings::default()
        });
        s.save().unwrap();
        let back = Store::load(&path);
        assert_eq!(back.machines(), ["m@h"]);
        assert_eq!(back.defaults().clipboard, Some(false));
        let _ = std::fs::remove_dir_all(dir);
    }
}
