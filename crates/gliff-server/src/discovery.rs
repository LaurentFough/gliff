//! Advertise the SSH endpoint, without opening capture or input devices.
use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn advertise(port: u16) -> Result<()> {
    // Only advertise a desktop that can actually be served by this user.
    hypr_wl::Target::default()
        .instance()
        .context("discovery requires a running Hyprland desktop")?;
    let user = Command::new("id").arg("-un").output()?.stdout;
    let user = String::from_utf8(user)?.trim().to_string();
    let hostname = std::fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "Linux".into());
    let hostname = hostname.trim();
    let platform = std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("ID="))
        .unwrap_or("linux")
        .trim_matches('"')
        .to_string();
    let args = advertisement(hostname, &user, &platform, port);
    // Replace this process: the only idle process is Avahi's small publisher.
    let error = Command::new("avahi-publish-service").args(args).exec();
    Err(error).context("publish Gliff service (install Avahi and start avahi-daemon)")
}

fn advertisement(hostname: &str, user: &str, platform: &str, port: u16) -> Vec<String> {
    let host: String = hostname.chars().take(30).collect();
    let label: String = user.chars().take(16).collect();
    vec![
        "--no-fail".into(),
        format!("Gliff on {host} ({label})"),
        "_gliff._tcp".into(),
        port.to_string(),
        "transport=ssh".into(),
        format!("protocol={}", gliff_proto::PROTOCOL_VERSION),
        format!("user={user}"),
        format!("platform={platform}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_ssh_metadata_as_literal_arguments() {
        let args = advertisement("office", "archogenesis", "omarchy", 2222);
        assert_eq!(
            &args[2..],
            &[
                "_gliff._tcp",
                "2222",
                "transport=ssh",
                "protocol=1",
                "user=archogenesis",
                "platform=omarchy"
            ]
        );
        assert!(args[1].contains("office"));
        let long = advertisement(&"a".repeat(200), &"u".repeat(32), "linux", 22);
        assert!(long[1].len() <= 63);
    }
}
