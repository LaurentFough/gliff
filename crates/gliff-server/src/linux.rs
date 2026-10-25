//! gliff-server: captures a Hyprland output, encodes it as Dual420 4:4:4, and
//! serves it to one client over the gliff protocol. `--stdio` (spawned by ssh)
//! or `--listen` (dev, localhost).

use crate::session;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncRead, AsyncWrite};

use hypr_wl::Target;

#[derive(Parser)]
#[command(
    name = "gliff-server",
    version,
    about = "Serve a Hyprland session over the gliff protocol"
)]
struct Cli {
    /// Advertise this logged-in desktop to nearby Gliff clients via Avahi.
    #[arg(long, conflicts_with_all = ["stdio", "listen", "headless", "output"])]
    advertise: bool,
    /// SSH port to advertise (default: 22); no new listening socket is opened.
    #[arg(long, requires = "advertise", value_parser = clap::value_parser!(u16).range(1..))]
    ssh_port: Option<u16>,
    /// Serve over stdin/stdout (spawned by ssh). stdout carries the protocol.
    #[arg(long, conflicts_with = "listen")]
    stdio: bool,
    /// Dev only: listen on a TCP address (no auth; localhost).
    #[arg(long)]
    listen: Option<String>,
    /// Allow an unauthenticated development listener on non-loopback addresses.
    #[arg(long, requires = "listen")]
    allow_remote: bool,
    /// Mirror an existing output: a name like `DP-1`, or `auto` for the
    /// focused one (the default).
    #[arg(long, conflicts_with = "headless")]
    output: Option<String>,
    /// Create a dedicated headless output sized and scaled to the client
    /// window instead of mirroring.
    #[arg(long)]
    headless: bool,
    /// Hyprland instance signature (default: newest).
    #[arg(long)]
    instance: Option<String>,
    /// DRM render node for Vulkan and GBM.
    #[arg(long)]
    render_node: Option<PathBuf>,
    /// Single 4:2:0 stream instead of 4:4:4 (lower bandwidth).
    #[arg(long)]
    low_bandwidth: bool,
    /// Maximum bitrate in bits per second (default: derived from size). The
    /// server adapts below it when the link shows queueing.
    #[arg(long)]
    bitrate: Option<u32>,
    /// Video pipeline: `gpu` (Vulkan compute + VA-API, falling back to the
    /// CPU when unavailable) or `cpu` (force OpenH264 on the CPU). Overrides
    /// the GLIFF_VIDEO environment variable.
    #[arg(long, value_parser = ["gpu", "cpu"])]
    video: Option<String>,
    /// Keep dual-stream 4:4:4 colour on the CPU pipeline, which defaults to
    /// a single 4:2:0 stream to halve the encode work.
    #[arg(long)]
    full_chroma: bool,
    /// Capture buffers: `auto` (dmabufs, falling back to wl_shm when the
    /// compositor offers no dmabuf or the render node cannot allocate),
    /// `dmabuf` (never wl_shm) or `shm` (the compositor reads each frame back
    /// into wl_shm; for a dmabuf that allocates but is never filled, such as
    /// a VM's virtio-gpu, and takes the CPU video pipeline). Overrides the
    /// GLIFF_CAPTURE environment variable.
    #[arg(long, value_parser = ["auto", "dmabuf", "shm"])]
    capture: Option<String>,
}

pub(crate) fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.advertise {
        return crate::discovery::advertise(cli.ssh_port.unwrap_or(22));
    }
    // In --stdio mode stdout IS the protocol: all logs go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    if cli.stdio {
        assert_stdout_is_free()?;
    }

    let target = Target {
        display: None,
        instance: cli.instance.clone(),
    };
    let render_node = hypr_capture::render_node_for_target(cli.render_node.as_deref(), &target);
    let cfg = session::Config {
        target,
        output: if cli.headless {
            None
        } else {
            Some(cli.output.clone().unwrap_or_else(|| "auto".into()))
        },
        render_node,
        low_bandwidth: cli.low_bandwidth,
        bitrate: cli.bitrate,
        video: gliff_sw::VideoMode::resolve(cli.video.as_deref()),
        full_chroma: cli.full_chroma,
        capture: hypr_capture::CaptureMode::resolve(cli.capture.as_deref()),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        if let Some(addr) = cli.listen {
            let listener = gliff_transport::bind_development_listener(&addr, cli.allow_remote)
                .await
                .with_context(|| format!("bind {addr}"))?;
            tracing::info!(%addr, "listening (dev mode, no auth)");
            let (stream, peer) = listener.accept().await?;
            tracing::info!(%peer, "client connected");
            stream.set_nodelay(true)?;
            let (rd, wr) = tokio::io::split(stream);
            serve(rd, wr, cfg).await
        } else {
            let rd = tokio::io::stdin();
            let wr = tokio::io::stdout();
            serve(rd, wr, cfg).await
        }
    })
}

async fn serve<R, W>(rd: R, wr: W, cfg: session::Config) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    match session::run(rd, wr, cfg).await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::error!(error = %e, "session ended with error");
            Err(e)
        }
    }
}

/// In --stdio mode nothing else may hold fd 1, or it would corrupt the wire.
fn assert_stdout_is_free() -> Result<()> {
    // A best-effort check: stdout must be a pipe or socket, not a tty.
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        anyhow::bail!("--stdio requires stdout to be a pipe (it is a terminal)");
    }
    Ok(())
}
