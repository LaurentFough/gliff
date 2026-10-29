//! Per-machine options for the macOS client, on the shared `gliff-settings`
//! model and `config.toml`, so a Mac and a Linux client agree on what
//! `[defaults]` and `[overrides."user@host"]` mean.
//!
//! The Mac offers three options: the screen (mirror or a private Linux
//! screen), sharing the clipboard, and the server command. The decode
//! pipeline is kept in the file for Linux but not offered here: the Mac
//! viewer always decodes on the CPU.
//!
//! The Swift windows (`native/Gliff.swift`) never touch the file. They get
//! JSON state from here and send edits back:
//!
//! - settings window: `settings_state` in, `settings_edit` out (see
//!   [`state_json`], [`apply_edit`]);
//! - viewer's Machine menu: `machine_state` in, `setting` out
//!   ([`machine_state`], [`apply_machine_setting`]).
//!
//! Every change is saved at once and announced on [`Shared::changed`], which a
//! running session watches to apply the clipboard live or reconnect.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{bail, ensure, Context, Result};
use gliff_settings::{Field, Resolved, Screen, Settings, Store, Video, DEFAULT_SERVER_BIN};
use serde_json::{json, Map, Value};
use tokio::sync::watch;

/// The options this client offers, in window order.
pub const OFFERED: [Field; 3] = [Field::Screen, Field::Clipboard, Field::ServerBin];
/// Longest server command accepted from the UI.
const MAX_SERVER_BIN: usize = 1024;

/// The store and its change signal, shared by the picker, the settings
/// window and the session on one thread (the client's `LocalSet`).
pub struct Shared {
    store: RefCell<Store>,
    /// Bumped after every saved change.
    changed: watch::Sender<u64>,
    /// Whether a settings window is open, so a second request focuses it
    /// instead of opening another.
    window_open: Cell<bool>,
}

pub type SharedRef = Rc<Shared>;

impl Shared {
    pub fn new(store: Store) -> SharedRef {
        let (changed, _) = watch::channel(0);
        Rc::new(Self {
            store: RefCell::new(store),
            changed,
            window_open: Cell::new(false),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn resolve(&self, machine: &str, cli: &Settings) -> Resolved {
        self.store.borrow().resolve(machine, cli)
    }

    pub fn machine(&self, machine: &str) -> Settings {
        self.store.borrow().machine(machine)
    }

    /// Record a connection; saved only when the machine is new.
    pub fn remember(&self, machine: &str) {
        let added = self.store.borrow_mut().remember(machine);
        if added {
            self.save_and_announce();
        }
    }

    /// Change one layer and save. `machine: None` is the defaults.
    pub fn edit(&self, machine: Option<&str>, f: impl FnOnce(&mut Settings)) {
        {
            let mut store = self.store.borrow_mut();
            match machine {
                Some(m) => {
                    let mut s = store.machine(m);
                    f(&mut s);
                    store.remember(m);
                    store.set_machine(m, s);
                }
                None => {
                    let mut s = store.defaults();
                    f(&mut s);
                    store.set_defaults(s);
                }
            }
        }
        self.save_and_announce();
    }

    fn save_and_announce(&self) {
        let result = self.store.borrow().save();
        if let Err(e) = result {
            tracing::warn!(error = %e, "cannot save settings");
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn state(&self) -> Value {
        state_json(&self.store.borrow())
    }

    /// The sound switch, shared with the Linux client (`muted` in the file).
    pub fn muted(&self) -> bool {
        self.store.borrow().muted()
    }

    /// Save the sound switch; announced like any other change.
    pub fn set_muted(&self, muted: bool) {
        if self.muted() == muted {
            return;
        }
        self.store.borrow_mut().set_muted(muted);
        self.save_and_announce();
    }
}

/// The command line's layer. `--output` is not here: naming a screen is a
/// concrete request, handled where the server arguments are built.
/// `video` is `--video`: `gpu` (VideoToolbox + Metal when the Mac has it)
/// or `cpu` (OpenH264).
pub fn cli_layer(
    headless: bool,
    no_clipboard: bool,
    server_bin: &str,
    video: Option<&str>,
) -> Settings {
    Settings {
        screen: headless.then_some(Screen::Own),
        clipboard: no_clipboard.then_some(false),
        decode: match video {
            Some("gpu") => Some(Video::Gpu),
            Some("cpu") => Some(Video::Cpu),
            _ => None,
        },
        server_bin: (server_bin.trim() != DEFAULT_SERVER_BIN && !server_bin.trim().is_empty())
            .then(|| server_bin.trim().to_string()),
    }
}

/// The settings key for a connection: the SSH address as typed. The port
/// is not part of it, matching the Linux client's machine names.
pub fn machine_key(host: &str) -> String {
    host.trim().to_string()
}

/// The UI's name for a screen mode (the picker has always said "private").
pub fn screen_ui(screen: Screen) -> &'static str {
    match screen {
        Screen::Mirror => "mirror",
        Screen::Own => "private",
    }
}

fn screen_from_ui(value: &str) -> Option<Screen> {
    match value {
        "mirror" => Some(Screen::Mirror),
        "private" | "own" => Some(Screen::Own),
        _ => None,
    }
}

/// One layer's offered options as JSON; unset options are absent.
fn layer_json(s: &Settings) -> Value {
    let mut m = Map::new();
    if let Some(screen) = s.screen {
        m.insert("screen".into(), screen_ui(screen).into());
    }
    if let Some(clipboard) = s.clipboard {
        m.insert("clipboard".into(), clipboard.into());
    }
    if let Some(bin) = &s.server_bin {
        m.insert("server_bin".into(), bin.clone().into());
    }
    Value::Object(m)
}

/// Everything the settings window shows.
pub fn state_json(store: &Store) -> Value {
    let builtin = Settings::default().resolve();
    let machines: Vec<Value> = store
        .machines()
        .into_iter()
        .map(|name| json!({"name": name, "settings": layer_json(&store.machine(&name))}))
        .collect();
    json!({
        "type": "settings_state",
        "path": store.path().map(|p| p.display().to_string()).unwrap_or_default(),
        "builtin": {
            "screen": screen_ui(builtin.screen),
            "clipboard": builtin.clipboard,
            "server_bin": builtin.server_bin,
        },
        "defaults": layer_json(&store.defaults()),
        "machines": machines,
    })
}

/// Set (`Some`) or clear (`None`) one offered option in a layer, from a
/// UI value. Rejects options the Mac does not offer and malformed values.
fn set_field(s: &mut Settings, field: Field, value: Option<&Value>) -> Result<()> {
    ensure!(
        OFFERED.contains(&field),
        "{} is not offered on macOS",
        field.name()
    );
    let Some(value) = value.filter(|v| !v.is_null()) else {
        s.clear(field);
        return Ok(());
    };
    match field {
        Field::Screen => {
            let v = value.as_str().context("screen must be a string")?;
            s.screen = Some(screen_from_ui(v).with_context(|| format!("unknown screen `{v}`"))?);
        }
        Field::Clipboard => {
            s.clipboard = Some(value.as_bool().context("clipboard must be true or false")?);
        }
        Field::ServerBin => {
            let v = value.as_str().context("server command must be a string")?;
            ensure!(v.len() <= MAX_SERVER_BIN, "server command is too long");
            ensure!(
                !v.chars().any(|c| c.is_control()),
                "server command cannot contain control characters"
            );
            //= Blank clears it (Settings::tidy), like the Linux window.
            s.server_bin = Some(v.trim().to_string());
            s.tidy();
        }
        Field::Decode => unreachable!("not offered"),
    }
    Ok(())
}

/// Apply a `settings_edit` event from the settings window:
/// `{"scope": "defaults"|"machine", "machine": name, "field": key, "value": v|null}`.
pub fn apply_edit(shared: &Shared, ev: &Value) -> Result<()> {
    let field = ev["field"]
        .as_str()
        .and_then(Field::from_name)
        .context("unknown setting")?;
    let machine = match ev["scope"].as_str() {
        Some("defaults") => None,
        Some("machine") => {
            let name = ev["machine"].as_str().map(machine_key).unwrap_or_default();
            ensure!(
                !name.is_empty() && name.len() <= 320,
                "invalid machine name"
            );
            Some(name)
        }
        _ => bail!("unknown settings scope"),
    };
    //= Validate on a copy first, so a bad value never reaches the file.
    let mut probe = Settings::default();
    set_field(&mut probe, field, ev.get("value"))?;
    shared.edit(machine.as_deref(), |s| {
        let _ = set_field(s, field, ev.get("value"));
    });
    Ok(())
}

/// Apply a `setting` event from the viewer's Machine menu to `machine`'s
/// override: `{"field": key, "value": v}`.
pub fn apply_machine_setting(shared: &Shared, machine: &str, ev: &Value) -> Result<()> {
    let edit = json!({
        "scope": "machine",
        "machine": machine,
        "field": ev["field"],
        "value": ev["value"],
    });
    apply_edit(shared, &edit)
}

/// What the viewer's Machine menu shows for the running session.
/// `screen_locked`: `--output` named a screen, so the menu cannot switch it.
pub fn machine_state(opts: &Resolved, screen_locked: bool) -> Value {
    json!({
        "type": "machine_state",
        "screen": screen_ui(opts.screen),
        "clipboard": opts.clipboard,
        "screen_locked": screen_locked,
    })
}

/// How a settings change reaches a running session.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    None,
    /// Only the clipboard changed: apply it in place.
    Clipboard(bool),
    /// Anything else the session was started with: reconnect.
    Reconnect,
}

/// Compare the options a session runs with to the new ones (the Linux
/// client's `apply_settings` rule: the clipboard is live, the rest
/// reconnects). `screen_locked` ignores screen changes under `--output`.
pub fn change(old: &Resolved, new: &Resolved, screen_locked: bool) -> Change {
    let screen = !screen_locked && old.screen != new.screen;
    //= The decode path is chosen when the session starts.
    if screen || old.server_bin != new.server_bin || old.decode != new.decode {
        Change::Reconnect
    } else if old.clipboard != new.clipboard {
        Change::Clipboard(new.clipboard)
    } else {
        Change::None
    }
}

/// Run the settings window until it closes, applying its edits. Only one
/// window at a time; a second call while it is open does nothing.
#[cfg(target_os = "macos")]
pub async fn window(shared: SharedRef) -> Result<()> {
    if shared.window_open.replace(true) {
        return Ok(());
    }
    let result = run_window(&shared).await;
    shared.window_open.set(false);
    result
}

/// No settings window off macOS (the protocol tests run on Linux).
#[cfg(not(target_os = "macos"))]
pub async fn window(shared: SharedRef) -> Result<()> {
    let _ = shared.window_open.get();
    Ok(())
}

#[cfg(target_os = "macos")]
async fn run_window(shared: &Shared) -> Result<()> {
    use crate::native;
    let mut child = native::spawn("settings", &[])?;
    let mut rd = child.stdout.take().context("settings window stdout")?;
    let mut wr = child.stdin.take().context("settings window stdin")?;
    native::write(
        &mut wr,
        native::EVENT,
        &serde_json::to_vec(&shared.state())?,
    )
    .await?;
    let mut changes = shared.subscribe();
    let result = async {
        loop {
            tokio::select! {
                packet = native::read(&mut rd) => {
                    let Ok((kind, bytes)) = packet else { break };
                    ensure!(kind == native::EVENT, "unexpected settings packet");
                    let ev: Value = serde_json::from_slice(&bytes)?;
                    match ev["type"].as_str() {
                        Some("settings_edit") => {
                            if let Err(e) = apply_edit(shared, &ev) {
                                tracing::warn!(error = %e, "settings edit refused");
                                let msg = json!({"type": "settings_error", "message": format!("{e:#}")});
                                native::write(&mut wr, native::EVENT, &serde_json::to_vec(&msg)?).await?;
                            }
                        }
                        Some("settings_close") => break,
                        _ => {}
                    }
                }
                //= Edits from here or the viewer's menu: show them.
                changed = changes.changed() => {
                    if changed.is_err() { break; }
                    native::write(&mut wr, native::EVENT, &serde_json::to_vec(&shared.state())?).await?;
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = child.kill().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> SharedRef {
        Shared::new(Store::in_memory())
    }

    #[test]
    fn the_command_line_layer_sets_only_what_was_given() {
        assert_eq!(
            cli_layer(false, false, "gliff-server", None),
            Settings::default()
        );
        assert_eq!(
            cli_layer(false, false, "gliff-server", Some("cpu")).decode,
            Some(Video::Cpu)
        );
        let s = cli_layer(true, true, " env X=1 gliff-server ", Some("gpu"));
        assert_eq!(s.decode, Some(Video::Gpu));
        assert_eq!(s.screen, Some(Screen::Own));
        assert_eq!(s.clipboard, Some(false));
        assert_eq!(s.server_bin.as_deref(), Some("env X=1 gliff-server"));
    }

    #[test]
    fn edits_land_in_the_right_layer_and_null_clears() {
        let sh = shared();
        apply_edit(
            &sh,
            &json!({"scope":"defaults","field":"clipboard","value":false}),
        )
        .unwrap();
        apply_edit(
            &sh,
            &json!({"scope":"machine","machine":"m@h","field":"screen","value":"private"}),
        )
        .unwrap();
        let r = sh.resolve("m@h", &Settings::default());
        assert_eq!((r.screen, r.clipboard), (Screen::Own, false));
        apply_edit(
            &sh,
            &json!({"scope":"machine","machine":"m@h","field":"screen","value":null}),
        )
        .unwrap();
        assert_eq!(sh.machine("m@h"), Settings::default());
        //= The machine stays listed after its override is cleared.
        assert_eq!(sh.state()["machines"][0]["name"], "m@h");
    }

    #[test]
    fn bad_edits_are_refused_without_touching_the_file() {
        let sh = shared();
        for ev in [
            json!({"scope":"machine","machine":"m@h","field":"decode","value":"cpu"}),
            json!({"scope":"machine","machine":"m@h","field":"screen","value":"sideways"}),
            json!({"scope":"machine","machine":"m@h","field":"clipboard","value":"yes"}),
            json!({"scope":"machine","machine":"","field":"clipboard","value":true}),
            json!({"scope":"elsewhere","field":"clipboard","value":true}),
            json!({"scope":"defaults","field":"server_bin","value":"a\nb"}),
        ] {
            assert!(apply_edit(&sh, &ev).is_err(), "{ev}");
        }
        assert!(sh.state()["machines"].as_array().unwrap().is_empty());
        assert_eq!(sh.state()["defaults"], json!({}));
    }

    #[test]
    fn a_blank_server_command_clears_it() {
        let sh = shared();
        apply_edit(&sh, &json!({"scope":"defaults","field":"server_bin","value":"gliff-server --low-bandwidth"})).unwrap();
        assert_eq!(
            sh.resolve("x", &Settings::default()).server_bin,
            "gliff-server --low-bandwidth"
        );
        apply_edit(
            &sh,
            &json!({"scope":"defaults","field":"server_bin","value":"  "}),
        )
        .unwrap();
        assert_eq!(
            sh.resolve("x", &Settings::default()).server_bin,
            "gliff-server"
        );
    }

    #[test]
    fn the_clipboard_applies_live_and_the_rest_reconnects() {
        let base = Settings::default().resolve();
        let mut r = base.clone();
        r.clipboard = false;
        assert_eq!(change(&base, &r, false), Change::Clipboard(false));
        let mut r = base.clone();
        r.screen = Screen::Own;
        assert_eq!(change(&base, &r, false), Change::Reconnect);
        assert_eq!(change(&base, &r, true), Change::None);
        let mut r = base.clone();
        r.decode = Video::Cpu;
        assert_eq!(change(&base, &r, false), Change::Reconnect);
        let mut r = base.clone();
        r.server_bin = "other".into();
        assert_eq!(change(&base, &r, true), Change::Reconnect);
        assert_eq!(change(&base, &base, false), Change::None);
    }

    #[test]
    fn changes_are_announced() {
        let sh = shared();
        let mut rx = sh.subscribe();
        assert!(!rx.has_changed().unwrap());
        sh.edit(None, |s| s.clipboard = Some(false));
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        sh.remember("m@h");
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        sh.remember("m@h");
        assert!(
            !rx.has_changed().unwrap(),
            "a known machine is not a change"
        );
    }

    #[test]
    fn the_menu_state_uses_the_picker_wording() {
        let mut r = Settings::default().resolve();
        r.screen = Screen::Own;
        let v = machine_state(&r, true);
        assert_eq!(v["screen"], "private");
        assert_eq!(v["screen_locked"], true);
        assert_eq!(v["clipboard"], true);
    }
}
