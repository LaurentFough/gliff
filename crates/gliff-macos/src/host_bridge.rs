//! Keep capture in a LaunchServices-launched app, so macOS checks Gliff's grant.
//! SSH only relays bounded packets over a private, per-connection Unix socket.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const DATA: u8 = 1;
const STATUS: u8 = 2;
const EOF: u8 = 3;
const LIMIT: usize = 32 * 1024;

#[derive(Serialize, Deserialize)]
struct Status {
    error: Option<String>,
}

async fn packet<R: AsyncRead + Unpin>(rd: &mut R) -> Result<(u8, Vec<u8>)> {
    let kind = rd.read_u8().await?;
    let len = rd.read_u32_le().await? as usize;
    ensure!(len <= LIMIT, "host bridge packet exceeds limit");
    let mut data = vec![0; len];
    rd.read_exact(&mut data).await?;
    Ok((kind, data))
}

async fn send<W: AsyncWrite + Unpin>(wr: &mut W, kind: u8, data: &[u8]) -> Result<()> {
    ensure!(data.len() <= LIMIT, "host bridge packet exceeds limit");
    wr.write_u8(kind).await?;
    wr.write_u32_le(data.len() as u32).await?;
    wr.write_all(data).await?;
    wr.flush().await?;
    Ok(())
}

async fn input<R, W>(mut rd: R, mut wr: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0; LIMIT];
    loop {
        let n = rd.read(&mut buffer).await?;
        if n == 0 {
            send(&mut wr, EOF, &[]).await?;
            wr.shutdown().await?;
            return Ok(());
        }
        send(&mut wr, DATA, &buffer[..n]).await?;
    }
}

async fn output<R, W>(mut rd: R, mut wr: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let (kind, data) = packet(&mut rd)
            .await
            .context("Gliff host app disconnected")?;
        match kind {
            DATA => {
                wr.write_all(&data).await?;
                wr.flush().await?;
            }
            STATUS => {
                let status: Status = serde_json::from_slice(&data)?;
                if let Some(error) = status.error {
                    bail!("{error}");
                }
                return Ok(());
            }
            _ => bail!("unexpected host bridge packet"),
        }
    }
}

#[cfg(target_os = "macos")]
pub async fn relay(config: &crate::server::Cli) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct SocketDir(PathBuf);
    impl Drop for SocketDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("s"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    // /tmp keeps the socket pathname below macOS's sockaddr_un length limit.
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = PathBuf::from(format!("/tmp/gliff-{}-{stamp:x}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let dir = SocketDir(dir);
    let path = dir.0.join("s");
    let listener = tokio::net::UnixListener::bind(&path)?;
    let exe = std::env::current_exe()?;
    let bundle = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());
    let app = bundle
        .filter(|p| p.join("Contents/Info.plist").is_file())
        .context("install Gliff.app with bin/install-macos before hosting")?;
    let status = tokio::process::Command::new("/usr/bin/open")
        .args(["-n", "-g", "-a"])
        .arg(app)
        .args(["--args", "--host-bridge"])
        .arg(&path)
        .kill_on_drop(true)
        .status()
        .await?;
    ensure!(status.success(), "could not launch the Gliff host app");
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
        .await
        .context("Gliff host app did not start in the logged-in desktop")??;
    send(&mut socket, STATUS, &serde_json::to_vec(config)?).await?;
    let (rd, wr) = socket.into_split();
    let mut inbound = tokio::task::spawn_local(input(tokio::io::stdin(), wr));
    let outbound = output(rd, tokio::io::stdout());
    tokio::pin!(outbound);
    let result = tokio::select! {
        result = &mut outbound => result,
        result = &mut inbound => match result? {
            Ok(()) => outbound.await,
            Err(error) => Err(error),
        },
    };
    inbound.abort();
    result
}

#[cfg(target_os = "macos")]
pub async fn app(path: &std::path::Path) -> Result<()> {
    use std::time::Duration;

    let mut socket = tokio::net::UnixStream::connect(path).await?;
    let (kind, config) =
        tokio::time::timeout(Duration::from_secs(10), packet(&mut socket)).await??;
    ensure!(kind == STATUS, "missing host configuration");
    let config: crate::server::Cli = serde_json::from_slice(&config)?;
    let (mut rd, mut wr) = socket.into_split();
    let (mut incoming, session_rd) = tokio::io::duplex(LIMIT * 2);
    let (session_wr, mut outgoing) = tokio::io::duplex(LIMIT * 2);
    let inbound = tokio::task::spawn_local(async move {
        loop {
            let (kind, bytes) = packet(&mut rd).await?;
            match kind {
                DATA => incoming.write_all(&bytes).await?,
                EOF if bytes.is_empty() => break,
                _ => bail!("unexpected host input packet"),
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let outbound = tokio::task::spawn_local(async move {
        let mut buffer = vec![0; LIMIT];
        loop {
            let n = outgoing.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            send(&mut wr, DATA, &buffer[..n]).await?;
        }
        Ok::<_, anyhow::Error>(wr)
    });
    let result = crate::server::run(session_rd, session_wr, &config).await;
    inbound.abort();
    let mut wr = outbound.await??;
    send(
        &mut wr,
        STATUS,
        &serde_json::to_vec(&Status {
            error: result.as_ref().err().map(|e| format!("{e:#}")),
        })?,
    )
    .await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_backpressure_eof_and_error() {
        let source = vec![42_u8; LIMIT * 3 + 7];
        let (wire_wr, mut wire_rd) = tokio::io::duplex(17);
        let source_copy = source.clone();
        let task = tokio::spawn(async move { input(source_copy.as_slice(), wire_wr).await });
        let mut received = Vec::new();
        loop {
            let (kind, bytes) = packet(&mut wire_rd).await.unwrap();
            if kind == EOF {
                assert!(bytes.is_empty());
                break;
            }
            assert_eq!(kind, DATA);
            received.extend(bytes);
        }
        assert_eq!(received, source);
        task.await.unwrap().unwrap();

        let (mut wire_wr, wire_rd) = tokio::io::duplex(17);
        let (sink_wr, mut sink_rd) = tokio::io::duplex(17);
        let task = tokio::spawn(output(wire_rd, sink_wr));
        send(&mut wire_wr, DATA, b"before error").await.unwrap();
        let mut bytes = vec![0; 12];
        sink_rd.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"before error");
        send(&mut wire_wr, STATUS, br#"{"error":"capture denied"}"#)
            .await
            .unwrap();
        assert!(task
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("capture denied"));
        let header = [DATA, 0xff, 0xff, 0xff, 0xff];
        assert!(packet(&mut header.as_slice()).await.is_err());
    }
}
