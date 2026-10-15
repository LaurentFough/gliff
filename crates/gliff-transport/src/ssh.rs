//! Spawn `gliff-server --stdio` over ssh and expose its stdio as a stream.

use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// How to reach the server host over ssh.
#[derive(Debug, Clone)]
pub struct SshTarget {
    /// `user@host` or an ssh config alias.
    pub host: String,
    /// Remote `gliff-server` path (default: `gliff-server` on PATH).
    pub server_bin: String,
    /// Extra args passed to the server after `--stdio`.
    pub server_args: Vec<String>,
    /// Extra ssh options (e.g. `-J jump`).
    pub ssh_args: Vec<String>,
}

impl SshTarget {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            server_bin: "gliff-server".into(),
            server_args: Vec::new(),
            ssh_args: Vec::new(),
        }
    }
}

/// The spawned ssh process and its piped stdio, joined into one duplex stream.
pub struct SshStream {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

/// Spawn ssh. Compression is off (video is already compressed) and the socket
/// asks for low-latency queuing. stderr is inherited so server logs reach the
/// client's terminal.
pub fn spawn_ssh(target: &SshTarget) -> std::io::Result<SshStream> {
    let mut cmd = ssh_command(target);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::inherit());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    Ok(SshStream {
        child,
        stdin,
        stdout,
    })
}

fn ssh_command(target: &SshTarget) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-T")
        .arg("-o")
        .arg("Compression=no")
        .arg("-o")
        .arg("IPQoS=lowdelay")
        .args(&target.ssh_args)
        .arg("--")
        .arg(&target.host)
        .arg(&target.server_bin)
        .arg("--stdio")
        .args(&target.server_args);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_cannot_supply_ssh_options() {
        let target = SshTarget::new("-oProxyCommand=touch /tmp/unwanted");
        let cmd = ssh_command(&target);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();
        let host = args.iter().position(|a| *a == target.host).unwrap();
        assert_eq!(args[host - 1], "--");
    }
}
