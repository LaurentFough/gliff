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
        }
        .validate()?;
    }
    Ok(hosts)
}

#[cfg(target_os = "macos")]
pub async fn choose() -> Result<Option<Selection>> {
    let mut child = crate::native::spawn("choose", &[])?;
    let mut rd = child.stdout.take().unwrap();
    let result = async {
        let (kind, bytes) = crate::native::read(&mut rd).await?;
        ensure!(kind == crate::native::EVENT, "invalid chooser response");
        let event: serde_json::Value = serde_json::from_slice(&bytes)?;
        match event["type"].as_str() {
            Some("cancel") => Ok(None),
            Some("connect") => {
                let selection: Selection = serde_json::from_value(event)?;
                selection.validate()?;
                Ok(Some(selection))
            }
            _ => anyhow::bail!("unexpected chooser response"),
        }
    }
    .await;
    let _ = child.kill().await;
    result
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
                port: 22
            }
            .validate()
            .is_ok());
        }
        for address in ["", "-oProxyCommand=x", "x\ny", "user@host command"] {
            assert!(Selection {
                host: address.into(),
                port: 22
            }
            .validate()
            .is_err());
        }
        assert!(hosts(&json!({"hosts":[]})).unwrap().is_empty());
    }
}
