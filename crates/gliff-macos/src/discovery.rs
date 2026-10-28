//! Native chooser messages and SSH target validation. Discovery never starts video.
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct Host {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub platform: String,
}

#[derive(Debug, Deserialize)]
pub struct Selection {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub screen: ScreenMode,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScreenMode {
    #[default]
    Mirror,
    Private,
}

impl ScreenMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Private => "private",
        }
    }
}

impl Selection {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.port > 0, "invalid SSH port");
        ensure!(
            !self.host.is_empty()
                && self.host.len() <= 320
                && !self.host.starts_with('-')
                && self
                    .host
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.:@[]%".contains(&c)),
            "enter an SSH address such as user@computer.local"
        );
        Ok(())
    }
}

pub fn hosts(event: &serde_json::Value) -> Result<Vec<Host>> {
    let hosts: Vec<Host> = serde_json::from_value(event["hosts"].clone())?;
    ensure!(hosts.len() <= 128, "too many discovered computers");
    for host in &hosts {
        ensure!(
            !host.host.is_empty()
                && !host.host.starts_with('-')
                && host.host.len() <= 253
                && host
                    .host
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
            "invalid advertised hostname"
        );
        let address = if host.user.is_empty() {
            host.host.clone()
        } else {
            ensure!(
                host.user.len() <= 64
                    && !host.user.starts_with('-')
                    && host
                        .user
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
                "invalid advertised SSH user"
            );
            format!("{}@{}", host.user, host.host)
        };
        Selection {
            host: address,
            port: host.port,
            screen: ScreenMode::Mirror,
        }
        .validate()?;
    }
    Ok(hosts)
}

/// What the picker needs to show each machine's screen mode: the resolved
/// mode per known machine (and for a new address), and which machines set
/// one themselves, so their choice beats a pre-settings `UserDefaults` value.
pub fn picker_settings(
    shared: &crate::settings::Shared,
    cli: &gliff_settings::Settings,
) -> serde_json::Value {
    use crate::settings::screen_ui;
    let state = shared.state();
    let mut screens = serde_json::Map::new();
    let mut pinned = Vec::new();
    for m in state["machines"].as_array().into_iter().flatten() {
        let Some(name) = m["name"].as_str() else {
            continue;
        };
        screens.insert(
            name.into(),
            screen_ui(shared.resolve(name, cli).screen).into(),
        );
        if m["settings"]["screen"].is_string() {
            pinned.push(serde_json::Value::from(name));
        }
    }
    serde_json::json!({
        "type": "machine_settings",
        //= A machine with no override: an empty name has none.
        "default_screen": screen_ui(shared.resolve("", cli).screen),
        "screens": screens,
        "pinned": pinned,
    })
}

/// Show the picker. Its screen menu follows the saved settings; a screen
/// the user picked there (or one migrated from the picker's old
/// `UserDefaults` store) is saved as that machine's override on Connect.
/// "Settings…" opens the settings window alongside it.
#[cfg(target_os = "macos")]
pub async fn choose(
    screen: Option<ScreenMode>,
    shared: &crate::settings::SharedRef,
    cli: &gliff_settings::Settings,
) -> Result<Option<Selection>> {
    let args = screen
        .map(|mode| vec![mode.as_str().into()])
        .unwrap_or_default();
    let mut child = crate::native::spawn("choose", &args)?;
    let mut rd = child.stdout.take().unwrap();
    let mut wr = child.stdin.take().unwrap();
    let mut changes = Some(shared.subscribe());
    let result = async {
        send_picker_settings(&mut wr, shared, cli).await?;
        loop {
            tokio::select! {
                packet = crate::native::read(&mut rd) => {
                    let (kind, bytes) = packet?;
                    ensure!(kind == crate::native::EVENT, "invalid chooser response");
                    let event: serde_json::Value = serde_json::from_slice(&bytes)?;
                    match event["type"].as_str() {
                        Some("cancel") => return Ok(None),
                        Some("open_settings") => {
                            let shared = shared.clone();
                            tokio::task::spawn_local(async move {
                                if let Err(e) = crate::settings::window(shared).await {
                                    tracing::warn!(error = %e, "settings window failed");
                                }
                            });
                        }
                        Some("connect") => {
                            let explicit = event["screen_explicit"].as_bool() == Some(true);
                            let selection: Selection = serde_json::from_value(event)?;
                            selection.validate()?;
                            if explicit && screen.is_none() {
                                let mode = match selection.screen {
                                    ScreenMode::Mirror => gliff_settings::Screen::Mirror,
                                    ScreenMode::Private => gliff_settings::Screen::Own,
                                };
                                let machine = crate::settings::machine_key(&selection.host);
                                shared.edit(Some(machine.as_str()), |s| s.screen = Some(mode));
                            }
                            return Ok(Some(selection));
                        }
                        _ => anyhow::bail!("unexpected chooser response"),
                    }
                }
                //= The settings window changed something: refresh the menu.
                changed = async { changes.as_mut().unwrap().changed().await }, if changes.is_some() => {
                    if changed.is_ok() {
                        send_picker_settings(&mut wr, shared, cli).await?;
                    } else {
                        changes = None;
                    }
                }
            }
        }
    }
    .await;
    let _ = child.kill().await;
    result
}

#[cfg(target_os = "macos")]
async fn send_picker_settings(
    wr: &mut tokio::process::ChildStdin,
    shared: &crate::settings::Shared,
    cli: &gliff_settings::Settings,
) -> Result<()> {
    let bytes = serde_json::to_vec(&picker_settings(shared, cli))?;
    crate::native::write(wr, crate::native::EVENT, &bytes).await
}

#[cfg(target_os = "macos")]
pub async fn list() -> Result<Vec<Host>> {
    let mut child = crate::native::spawn("discover", &[])?;
    let mut rd = child.stdout.take().unwrap();
    let result = async {
        let (kind, bytes) = tokio::time::timeout(
            std::time::Duration::from_secs(12),
            crate::native::read(&mut rd),
        )
        .await
        .context("network discovery timed out")??;
        ensure!(kind == crate::native::EVENT, "invalid discovery response");
        let event: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(
            event["type"] == "hosts",
            "{}",
            event["message"]
                .as_str()
                .unwrap_or("network discovery failed")
        );
        hosts(&event)
    }
    .await;
    let _ = child.kill().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn untrusted_network_metadata_cannot_become_ssh_options() {
        let mut event = json!({"hosts":[{"name":"Office", "host":"office.local", "port":2222, "user":"archogenesis", "platform":"omarchy"}]});
        assert_eq!(hosts(&event).unwrap()[0].port, 2222);
        for bad in [
            "-oProxyCommand=touch /tmp/no",
            "user;cmd",
            "x@evil",
            "$(cmd)",
        ] {
            event["hosts"][0]["user"] = json!(bad);
            assert!(hosts(&event).is_err());
        }
        event["hosts"][0]["user"] = json!("");
        event["hosts"][0]["host"] = json!("-evil.local");
        assert!(hosts(&event).is_err());
        event["hosts"][0]["host"] = json!("office.local");
        event["hosts"][0]["port"] = json!(0);
        assert!(hosts(&event).is_err());
    }

    #[test]
    fn manual_aliases_ipv6_and_cancellation_are_distinct_from_empty_hosts() {
        for address in [
            "office",
            "me@office.local",
            "me@fe80::1%en0",
            "me@100.98.217.74",
        ] {
            assert!(Selection {
                host: address.into(),
                port: 22,
                screen: ScreenMode::Mirror,
            }
            .validate()
            .is_ok());
        }
        for address in ["", "-oProxyCommand=x", "x\ny", "user@host command"] {
            assert!(Selection {
                host: address.into(),
                port: 22,
                screen: ScreenMode::Mirror,
            }
            .validate()
            .is_err());
        }
        assert!(hosts(&json!({"hosts":[]})).unwrap().is_empty());
    }

    #[test]
    fn private_screen_selection_is_explicit_and_older_pickers_mirror() {
        let old: Selection = serde_json::from_value(json!({"host":"host", "port":22})).unwrap();
        assert_eq!(old.screen, ScreenMode::Mirror);
        let private: Selection =
            serde_json::from_value(json!({"host":"host", "port":22,"screen":"private"})).unwrap();
        assert_eq!(private.screen, ScreenMode::Private);
        assert!(serde_json::from_value::<Selection>(
            json!({"host":"host", "port":22,"screen":"unknown"})
        )
        .is_err());
    }

    #[test]
    fn the_picker_gets_saved_screens_and_which_machines_set_one() {
        use gliff_settings::{Screen, Settings, Store};
        let shared = crate::settings::Shared::new(Store::in_memory());
        shared.edit(None, |s| s.screen = Some(Screen::Own));
        shared.edit(Some("pinned@h"), |s| s.screen = Some(Screen::Mirror));
        shared.remember("plain@h");
        let v = picker_settings(&shared, &Settings::default());
        assert_eq!(v["type"], "machine_settings");
        assert_eq!(v["default_screen"], "private");
        assert_eq!(v["screens"]["pinned@h"], "mirror");
        assert_eq!(v["screens"]["plain@h"], "private");
        assert_eq!(v["pinned"], serde_json::json!(["pinned@h"]));
    }
}
