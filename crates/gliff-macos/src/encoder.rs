//! Hardware H.264 when available; OpenH264 for fallback and dual-stream colour.
use anyhow::{ensure, Result};

pub fn hardware_allowed(cpu: bool, dual: bool) -> bool {
    !cpu && !dual
}

pub fn encoded_frame(bytes: Vec<u8>) -> Result<gliff_sw::EncodedFrame> {
    ensure!(bytes.len() >= 9, "truncated native H.264 packet");
    let flags = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    ensure!(flags <= 1, "unknown native H.264 flags");
    ensure!(bytes[4..8] == [0, 0, 0, 1], "native H.264 is not Annex B");
    Ok(gliff_sw::EncodedFrame {
        main: bytes[4..].to_vec(),
        aux: None,
        keyframe: flags == 1,
    })
}

#[cfg(target_os = "macos")]
pub struct Hardware {
    // Retaining this guard kills the helper when the encoder is replaced.
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    settings: gliff_sw::EncoderSettings,
}

#[cfg(target_os = "macos")]
impl Hardware {
    pub async fn new(settings: gliff_sw::EncoderSettings) -> Result<Self> {
        use crate::native;
        let mut child = native::spawn(
            "encode",
            &[
                settings.width.to_string(),
                settings.height.to_string(),
                settings.bitrate.to_string(),
                settings.framerate.to_string(),
            ],
        )?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing encoder input pipe"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing encoder output pipe"))?;
        let (kind, bytes) =
            tokio::time::timeout(std::time::Duration::from_secs(5), native::read(&mut stdout))
                .await??;
        ensure!(kind == native::EVENT, "missing native encoder status");
        let event: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(
            event["type"] == "encoder" && event["hardware"] == true,
            "{}",
            event["message"]
                .as_str()
                .unwrap_or("hardware encoder unavailable")
        );
        Ok(Self {
            _child: child,
            stdin,
            stdout,
            settings,
        })
    }

    pub async fn encode(
        &mut self,
        pixels: &[u8],
        keyframe: bool,
    ) -> Result<gliff_sw::EncodedFrame> {
        use crate::native;
        ensure!(
            pixels.len() as u64 == self.settings.width as u64 * self.settings.height as u64 * 4,
            "invalid encoder pixel length"
        );
        let mut bytes = Vec::with_capacity(16 + pixels.len());
        for n in [
            self.settings.width,
            self.settings.height,
            1000,
            u32::from(keyframe),
        ] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        bytes.extend_from_slice(pixels);
        // One request at a time keeps both VideoToolbox and the bridge bounded.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            native::write(&mut self.stdin, native::ENCODE, &bytes).await?;
            let (kind, encoded) = native::read(&mut self.stdout).await?;
            if kind == native::EVENT {
                let event: serde_json::Value = serde_json::from_slice(&encoded)?;
                anyhow::bail!("{}", event["message"]);
            }
            ensure!(kind == native::H264, "unexpected native encoder packet");
            encoded_frame(encoded)
        })
        .await?
    }
}

#[cfg(target_os = "macos")]
pub enum Encoder {
    Hardware(Box<Hardware>),
    Software(Box<gliff_sw::Encoder>),
}

#[cfg(target_os = "macos")]
impl Encoder {
    pub async fn new(settings: gliff_sw::EncoderSettings, cpu: bool, dual: bool) -> Result<Self> {
        if hardware_allowed(cpu, dual) {
            match Hardware::new(settings.clone()).await {
                Ok(encoder) => {
                    tracing::info!("using VideoToolbox hardware H.264 encoder");
                    return Ok(Self::Hardware(Box::new(encoder)));
                }
                Err(error) => {
                    tracing::warn!(%error, "hardware encoder unavailable; using OpenH264")
                }
            }
        }
        Ok(Self::Software(Box::new(gliff_sw::Encoder::new(
            settings, dual,
        )?)))
    }

    pub fn pipeline(&self) -> gliff_proto::VideoPipeline {
        match self {
            Self::Hardware(_) => gliff_proto::VideoPipeline::Gpu,
            Self::Software(_) => gliff_proto::VideoPipeline::Cpu,
        }
    }

    pub async fn encode(
        &mut self,
        pixels: &[u8],
        keyframe: bool,
    ) -> Result<gliff_sw::EncodedFrame> {
        match self {
            Self::Software(encoder) => Ok(encoder.encode_bgra(pixels, keyframe)?),
            Self::Hardware(encoder) => match encoder.encode(pixels, keyframe).await {
                Ok(frame) => Ok(frame),
                Err(error) => {
                    tracing::warn!(%error, "hardware encoding failed; switching to OpenH264");
                    let mut software = gliff_sw::Encoder::new(encoder.settings.clone(), false)?;
                    let frame = software.encode_bgra(pixels, true)?;
                    *self = Self::Software(Box::new(software));
                    Ok(frame)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_policy_and_annex_b_validation() {
        assert!(hardware_allowed(false, false));
        assert!(!hardware_allowed(true, false));
        assert!(!hardware_allowed(false, true));
        let packet = vec![1, 0, 0, 0, 0, 0, 0, 1, 0x65, 42];
        let encoded = encoded_frame(packet.clone()).unwrap();
        assert!(encoded.keyframe);
        assert_eq!(encoded.main, &packet[4..]);
        assert!(encoded.aux.is_none());
        assert!(encoded_frame(vec![0; 8]).is_err());
        let mut bad = packet.clone();
        bad[0] = 2;
        assert!(encoded_frame(bad).is_err());
        let mut bad = packet;
        bad[7] = 2;
        assert!(encoded_frame(bad).is_err());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_hardware_keyframes_decode() -> Result<()> {
        let settings = gliff_sw::EncoderSettings {
            width: 64,
            height: 64,
            bitrate: 1_000_000,
            framerate: 30,
        };
        let mut encoder = match Hardware::new(settings).await {
            Ok(encoder) => encoder,
            Err(error) if std::env::var_os("GLIFF_REQUIRE_HARDWARE_ENCODER").is_none() => {
                eprintln!("hardware not available on this runner: {error}");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let mut decoder = gliff_sw::Decoder::new(false)?;
        let mut last_decoded = None;
        let pixels: Vec<u8> = [40, 120, 200, 255].repeat(64 * 64);
        for i in 0..8 {
            let frame = encoder.encode(&pixels, i == 0 || i == 7).await?;
            if i == 0 || i == 7 {
                ensure!(frame.keyframe, "forced IDR missing");
            }
            if let Some(decoded) = decoder.decode(&frame.main, &[])? {
                ensure!(
                    decoded.width == 64 && decoded.height == 64,
                    "wrong decoded extent"
                );
                last_decoded = Some(decoded);
            }
        }
        if let Some(decoded) = decoder.flush()? {
            last_decoded = Some(decoded);
        }
        let decoded =
            last_decoded.ok_or_else(|| anyhow::anyhow!("encoder produced no decodable frame"))?;
        ensure!(
            decoded.pixels[2].abs_diff(200) < 15 && decoded.pixels[0].abs_diff(40) < 15,
            "hardware colour conversion differs"
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn encoder_failure_falls_back_with_keyframe() -> Result<()> {
        let settings = gliff_sw::EncoderSettings {
            width: 64,
            height: 64,
            bitrate: 1_000_000,
            framerate: 30,
        };
        let mut encoder = Encoder::new(settings, false, false).await?;
        if let Encoder::Hardware(hardware) = &mut encoder {
            hardware._child.kill().await?;
            hardware._child.wait().await?;
        } else if std::env::var_os("GLIFF_REQUIRE_HARDWARE_ENCODER").is_some() {
            anyhow::bail!("hardware required for this failure test");
        }
        let pixels = [40, 120, 200, 255].repeat(64 * 64);
        let frame = encoder.encode(&pixels, false).await?;
        ensure!(
            encoder.pipeline() == gliff_proto::VideoPipeline::Cpu && frame.keyframe,
            "fallback must announce CPU and start with a keyframe"
        );
        let mut decoder = gliff_sw::Decoder::new(false)?;
        let decoded = decoder
            .decode(&frame.main, &[])?
            .or(decoder.flush()?)
            .ok_or_else(|| anyhow::anyhow!("fallback frame was not decodable"))?;
        ensure!(
            decoded.width == 64 && decoded.height == 64,
            "wrong fallback extent"
        );
        Ok(())
    }
}
