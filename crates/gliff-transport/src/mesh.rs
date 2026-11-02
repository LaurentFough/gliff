//! Machines on the mesh VPNs this computer is part of, for the connection
//! window: Tailscale (and Headscale, which uses the same client) and
//! Rayfish. mDNS discovery stays on one LAN; these list peers anywhere on
//! the mesh.
//!
//! Each source runs its own status command with JSON output and a short
//! timeout, and is skipped when the command is missing, fails, or prints
//! something that is not its status (another program called `ray`, say).
//! Nothing is sent anywhere: the commands only read the local daemon.
//!
//! - Tailscale / Headscale: `tailscale status --json`; peers with their
//!   MagicDNS name (else first Tailscale IP), online flag and OS. Only Linux
//!   and macOS peers are listed (the systems gliff hosts run on).
//! - Rayfish: `ray status --json`; each network's peers as
//!   `HOSTNAME.NETWORK.ray` (Rayfish Magic DNS; else the peer's IPv6), online
//!   unless Rayfish marked them offline after a failed reach.
//!
//! Refs: <https://tailscale.com/kb/1080/cli>,
//! <https://github.com/rayfish/rayfish> (`ray status --json`,
//! `ray-proto/src/ipc.rs` NetworkStatus / PeerStatus).
//!
//! Version: 1.0.0

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

/// One machine on a mesh VPN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshPeer {
    /// The mesh it is on: "Tailscale" or "Rayfish".
    pub source: &'static str,
    /// The source's network name (Rayfish), if it has several.
    pub network: Option<String>,
    /// The machine's own name.
    pub name: String,
    /// What to put after `user@`: a DNS name or an address.
    pub address: String,
    /// Reachable now, as far as the source knows.
    pub online: bool,
    /// The machine's system, when the source says.
    pub os: Option<String>,
}

/// A mesh VPN client: its program, where it may be installed (GUI apps on
/// macOS get a minimal PATH), its JSON status arguments, and the parser.
pub struct Source {
    pub name: &'static str,
    pub program: &'static str,
    pub places: &'static [&'static str],
    pub args: &'static [&'static str],
    pub parse: fn(&Value) -> Vec<MeshPeer>,
}

/// Every source this build knows.
pub const SOURCES: &[Source] = &[
    Source {
        name: "Tailscale",
        program: "tailscale",
        places: &[
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
            "/opt/homebrew/bin/tailscale",
            "/usr/local/bin/tailscale",
            "/usr/bin/tailscale",
        ],
        args: &["status", "--json"],
        parse: parse_tailscale,
    },
    Source {
        name: "Rayfish",
        program: "ray",
        places: &[
            "/usr/local/bin/ray",
            "/opt/homebrew/bin/ray",
            "~/.local/bin/ray",
            "/usr/bin/ray",
        ],
        args: &["status", "--json"],
        parse: parse_rayfish,
    },
];

/// An address the clients accept (the connection window's rule): letters,
/// digits and `._:-`, so a peer's name cannot smuggle ssh options or shell.
pub fn safe_address(address: &str) -> bool {
    !address.is_empty()
        && !address.starts_with('-')
        && address.len() <= 255
        && address
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
}

/// `tailscale status --json`: the `Peer` map. Nothing when the backend is
/// not running (logged out, stopped).
pub fn parse_tailscale(v: &Value) -> Vec<MeshPeer> {
    if v["BackendState"].as_str().is_some_and(|s| s != "Running") {
        return Vec::new();
    }
    let Some(peers) = v["Peer"].as_object() else {
        return Vec::new();
    };
    peers
        .values()
        .filter_map(|p| {
            let os = p["OS"].as_str().unwrap_or_default().to_string();
            //= gliff hosts are Linux (Hyprland) or macOS.
            if !matches!(os.to_ascii_lowercase().as_str(), "linux" | "macos") {
                return None;
            }
            let dns = p["DNSName"]
                .as_str()
                .unwrap_or_default()
                .trim_end_matches('.');
            let ip = p["TailscaleIPs"]
                .as_array()
                .and_then(|ips| ips.first())
                .and_then(Value::as_str)
                .unwrap_or_default();
            let address = if dns.is_empty() { ip } else { dns }.to_string();
            let name = p["HostName"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&address)
                .to_string();
            Some(MeshPeer {
                source: "Tailscale",
                network: None,
                name,
                address,
                online: p["Online"].as_bool().unwrap_or(false),
                os: Some(os),
            })
        })
        .filter(|p| safe_address(&p.address))
        .collect()
}

/// `ray status --json`: every network's peers. A peer is online unless its
/// state is "offline" (Rayfish's "idle" means reachable on demand).
pub fn parse_rayfish(v: &Value) -> Vec<MeshPeer> {
    let Some(networks) = v["networks"].as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for net in networks {
        let network = net["name"].as_str().unwrap_or_default();
        for p in net["peers"].as_array().into_iter().flatten() {
            if p["incompatible"].as_bool() == Some(true) {
                continue;
            }
            let hostname = p["hostname"].as_str().filter(|s| !s.is_empty());
            let address = match hostname {
                Some(h) if !network.is_empty() => format!("{h}.{network}.ray"),
                _ => p["ipv6"].as_str().unwrap_or_default().to_string(),
            };
            let online = !p["state"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("offline"));
            out.push(MeshPeer {
                source: "Rayfish",
                network: (!network.is_empty()).then(|| network.to_string()),
                name: hostname.unwrap_or(&address).to_string(),
                address,
                online,
                os: None,
            });
        }
    }
    out.retain(|p| safe_address(&p.address));
    out
}

/// The source's program: on PATH, else one of its usual places.
pub fn find_program(source: &Source) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(source.program))
            .find(|p| is_executable(p))
    });
    on_path.or_else(|| {
        source
            .places
            .iter()
            .filter_map(|p| match (p.strip_prefix("~/"), &home) {
                (Some(rest), Some(h)) => Some(h.join(rest)),
                (Some(_), None) => None,
                (None, _) => Some(PathBuf::from(p)),
            })
            .find(|p| is_executable(p))
    })
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Run one source's status command, giving up after `timeout`.
pub fn query(source: &Source, timeout: Duration) -> Vec<MeshPeer> {
    let Some(program) = find_program(source) else {
        return Vec::new();
    };
    let child = Command::new(&program)
        .args(source.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(child) = child else {
        tracing::debug!(source = source.name, "mesh status did not start");
        return Vec::new();
    };
    let (tx, rx) = mpsc::channel();
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(timeout) {
        Ok(Ok(out)) if out.status.success() => out.stdout,
        Ok(_) => {
            tracing::debug!(source = source.name, "mesh status failed");
            return Vec::new();
        }
        Err(_) => {
            tracing::debug!(source = source.name, "mesh status timed out");
            //= The waiting thread reaps it once it exits.
            let _ = Command::new("kill").arg(pid.to_string()).status();
            return Vec::new();
        }
    };
    match serde_json::from_slice::<Value>(&output) {
        Ok(v) => {
            let peers = (source.parse)(&v);
            tracing::debug!(source = source.name, peers = peers.len(), "mesh peers");
            peers
        }
        Err(_) => Vec::new(),
    }
}

/// Every source at once: online peers first, then by mesh and name, one
/// entry per address. Blocking; runs the sources in parallel.
pub fn discover(timeout: Duration) -> Vec<MeshPeer> {
    let handles: Vec<_> = SOURCES
        .iter()
        .map(|s| std::thread::spawn(move || query(s, timeout)))
        .collect();
    let mut peers: Vec<MeshPeer> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap_or_default())
        .collect();
    sort_and_dedup(&mut peers);
    peers
}

fn sort_and_dedup(peers: &mut Vec<MeshPeer>) {
    peers.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then(a.source.cmp(b.source))
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let mut seen = std::collections::HashSet::new();
    peers.retain(|p| seen.insert(p.address.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tailscale_lists_linux_and_mac_peers_by_magicdns_name() {
        let v = json!({
            "BackendState": "Running",
            "Self": {"HostName": "me", "DNSName": "me.tail1.ts.net.", "OS": "macOS"},
            "Peer": {
                "nodekey:a": {"HostName": "omarchy", "DNSName": "omarchy.tail1.ts.net.", "OS": "linux",
                              "TailscaleIPs": ["100.64.0.2", "fd7a::2"], "Online": true},
                "nodekey:b": {"HostName": "phone", "DNSName": "phone.tail1.ts.net.", "OS": "iOS",
                              "TailscaleIPs": ["100.64.0.3"], "Online": true},
                "nodekey:c": {"HostName": "mini", "DNSName": "", "OS": "macOS",
                              "TailscaleIPs": ["100.64.0.4"], "Online": false},
                "nodekey:d": {"HostName": "-oProxyCommand=x", "DNSName": "-oProxyCommand=x.", "OS": "linux",
                              "TailscaleIPs": [], "Online": true}
            }
        });
        let mut peers = parse_tailscale(&v);
        sort_and_dedup(&mut peers);
        assert_eq!(peers.len(), 2, "{peers:?}");
        assert_eq!(peers[0].address, "omarchy.tail1.ts.net");
        assert!(peers[0].online);
        assert_eq!(peers[1].address, "100.64.0.4", "no MagicDNS: the IP");
        assert!(!peers[1].online);
        assert!(parse_tailscale(&json!({"BackendState": "Stopped", "Peer": {}})).is_empty());
    }

    #[test]
    fn rayfish_lists_each_networks_peers_by_magic_dns() {
        let v = json!({
            "networks": [
                {"name": "lab", "peers": [
                    {"hostname": "omarchy", "ipv6": "fd00::1", "state": "Active"},
                    {"hostname": null, "ipv6": "fd00::2", "state": "idle"},
                    {"hostname": "old", "ipv6": "fd00::3", "state": "Offline"},
                    {"hostname": "new", "ipv6": "fd00::4", "state": "Active", "incompatible": true}
                ]}
            ]
        });
        let peers = parse_rayfish(&v);
        assert_eq!(peers.len(), 3, "{peers:?}");
        assert_eq!(peers[0].address, "omarchy.lab.ray");
        assert_eq!(peers[0].network.as_deref(), Some("lab"));
        assert!(peers[0].online);
        assert_eq!(peers[1].address, "fd00::2");
        assert!(peers[1].online, "idle is reachable on demand");
        assert!(!peers[2].online);
        //= Another program's JSON (e.g. a different `ray`) yields nothing.
        assert!(parse_rayfish(&json!({"cluster": {}})).is_empty());
    }

    #[test]
    fn addresses_are_checked() {
        assert!(safe_address("omarchy.tail1.ts.net") && safe_address("fd00::2"));
        assert!(!safe_address("-oProxyCommand=x") && !safe_address("a b") && !safe_address(""));
    }

    #[test]
    fn missing_programs_are_skipped() {
        let s = Source {
            name: "None",
            program: "gliff-no-such-mesh-cli",
            places: &["/nonexistent/gliff-no-such-mesh-cli"],
            args: &[],
            parse: parse_rayfish,
        };
        assert!(query(&s, Duration::from_millis(100)).is_empty());
    }
}
