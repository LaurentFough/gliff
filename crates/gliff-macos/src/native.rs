//! The local bridge uses a bounded packet: kind:u8, length:u32 LE, payload.
//! Pixels are BGRA preceded by width, height, and scale_milli (u32 LE).

use anyhow::{ensure, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PIXELS: u8 = 1;
pub const EVENT: u8 = 2;
pub const H264: u8 = 3;
pub const ENCODE: u8 = 4;
pub const MAX_PACKET: usize = 64 * 1024 * 1024;

pub async fn read<R: AsyncRead + Unpin>(rd: &mut R) -> Result<(u8, Vec<u8>)> {
    let kind = rd.read_u8().await?;
    let len = rd.read_u32_le().await? as usize;
    ensure!(len <= MAX_PACKET, "native packet exceeds limit");
    let mut payload = vec![0; len];
    rd.read_exact(&mut payload).await?;
    Ok((kind, payload))
}

pub async fn write<W: AsyncWrite + Unpin>(wr: &mut W, kind: u8, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= MAX_PACKET, "native packet exceeds limit");
    wr.write_u8(kind).await?;
    wr.write_u32_le(bytes.len() as u32).await?;
    wr.write_all(bytes).await?;
    wr.flush().await?;
    Ok(())
}

pub fn geometry(bytes: &[u8]) -> Result<(u32, u32, u32)> {
    ensure!(bytes.len() >= 12, "truncated native frame");
    let u32_at = |i| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
    let (w, h, scale) = (u32_at(0), u32_at(4), u32_at(8));
    ensure!(w > 0 && h > 0 && scale > 0, "invalid native frame geometry");
    let len = (w as u64)
        .checked_mul(h as u64)
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| n.checked_add(12))
        .ok_or_else(|| anyhow::anyhow!("native frame size overflow"))?;
    ensure!(len == bytes.len() as u64, "native pixel length mismatch");
    Ok((w, h, scale))
}

#[cfg(target_os = "macos")]
pub fn install_helper(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/gliff-native"));
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, bytes)?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o700))?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn helper_path() -> Result<std::path::PathBuf> {
    // Concurrent callers in this process share the same staging filename.
    // Serialize installation so one cannot rename another caller's file.
    static INSTALL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use std::os::unix::fs::PermissionsExt;
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };
    let embedded = include_bytes!(concat!(env!("OUT_DIR"), "/gliff-native"));
    let exe = std::env::current_exe()?;
    let sibling = exe.parent().unwrap().join("gliff-native");
    let path = if sibling.is_file() {
        sibling
    } else {
        let _install = INSTALL
            .lock()
            .map_err(|_| anyhow::anyhow!("native helper installation lock poisoned"))?;
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is unset"))?;
        let dir = std::path::PathBuf::from(home).join("Library/Caches/gliff/native");
        std::fs::create_dir_all(&dir)?;
        let mut hash = DefaultHasher::new();
        embedded.hash(&mut hash);
        let dest = dir.join(format!("gliff-native-{:x}", hash.finish()));
        if !dest.is_file() {
            let temp = dir.join(format!(".native-{}", std::process::id()));
            std::fs::write(&temp, embedded)?;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o700))?;
            std::fs::rename(temp, &dest)?;
        }
        dest
    };
    Ok(path)
}

#[cfg(target_os = "macos")]
pub fn spawn(mode: &str, args: &[String]) -> Result<tokio::process::Child> {
    Ok(tokio::process::Command::new(helper_path()?)
        .arg(mode)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?)
}

// Protocol tests run without launching a native window on Linux.
#[cfg(all(test, not(target_os = "macos")))]
pub fn spawn(_mode: &str, _args: &[String]) -> Result<tokio::process::Child> {
    anyhow::bail!("native windows require macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn packet_limit_and_partial_io() {
        let (mut wr, mut rd) = tokio::io::duplex(3);
        let task = tokio::spawn(async move { write(&mut wr, EVENT, b"event").await });
        assert_eq!(read(&mut rd).await.unwrap(), (EVENT, b"event".to_vec()));
        task.await.unwrap().unwrap();
        let header = [EVENT, 255, 255, 255, 255];
        assert!(read(&mut header.as_slice()).await.is_err());
    }

    #[test]
    fn rejects_truncated_and_overflowing_pixels() {
        assert!(geometry(&[0; 11]).is_err());
        let mut b = Vec::new();
        for n in [2_u32, 2, 1000] {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b.extend_from_slice(&[0; 16]);
        assert_eq!(geometry(&b).unwrap(), (2, 2, 1000));
        b[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        b[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(geometry(&b).is_err());
    }
}
