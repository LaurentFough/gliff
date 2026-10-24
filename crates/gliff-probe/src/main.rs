//! Environment probe: protocols, outputs, Vulkan, codec round-trip, capture,
//! and input injection. Every check prints PASS/FAIL lines.

#![forbid(unsafe_code)]

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, QueueHandle};

use gliff_proto::color::{bgra_to_yuv444, downscale_bgra, psnr, yuv444_to_bgra};
use gliff_sw::VideoMode;
use gliff_vk::{Decoder, DmabufPlane, EncodedFrame, Encoder, EncoderSettings, Gpu};
use hypr_capture::{CaptureConfig, CaptureEvent, CaptureMode, Capturer};
use hypr_input::{keys, Input, InputConfig, InputEvent};
use hypr_wl::Target;

#[derive(Parser)]
#[command(
    name = "gliff-probe",
    version,
    about = "Check that this machine can run gliff"
)]
struct Cli {
    /// Wayland socket name (defaults to WAYLAND_DISPLAY, then the Hyprland instance)
    #[arg(long, global = true)]
    display: Option<String>,
    /// Hyprland instance signature
    #[arg(long, global = true)]
    instance: Option<String>,
    /// DRM render node for GBM and Vulkan
    #[arg(long, global = true)]
    render_node: Option<PathBuf>,
    /// Video pipeline to probe: `gpu` (Vulkan compute + VA-API) or `cpu`
    /// (OpenH264).
    /// Overrides the GLIFF_VIDEO environment variable.
    #[arg(long, global = true, value_parser = ["gpu", "cpu"])]
    video: Option<String>,
    /// Capture buffers for `capture` and `pipeline`: `auto` (dmabufs, with
    /// the server's wl_shm fallbacks), `dmabuf` or `shm` (the compositor
    /// reads each frame back; CPU pipeline only).
    /// Overrides the GLIFF_CAPTURE environment variable.
    #[arg(long, global = true, value_parser = ["auto", "dmabuf", "shm"])]
    capture: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List Wayland globals and check the ones gliff needs
    Protocols,
    /// List outputs (Wayland view and hyprctl view)
    Outputs,
    /// Report Hyprland permission settings that can block capture
    Permissions,
    /// GPU tier: the Vulkan compute device and the VA-API H.264 codec
    #[command(alias = "vulkan")]
    Gpu,
    /// The GPU hand-off: a VA-API surface written by the Vulkan split
    /// shader and read back through the driver
    Surfaces {
        #[arg(long, default_value_t = 640)]
        width: u32,
        #[arg(long, default_value_t = 360)]
        height: u32,
    },
    /// Encode and decode synthetic BGRA frames end to end on the GPU
    Roundtrip {
        #[arg(long, default_value_t = 640)]
        width: u32,
        #[arg(long, default_value_t = 360)]
        height: u32,
        #[arg(long, default_value_t = 10)]
        frames: usize,
        /// Single 4:2:0 stream instead of Dual420 4:4:4.
        #[arg(long)]
        single: bool,
        /// Bits per second per stream (default: 4x the server default, as
        /// the synthetic stripes are a worst case for 4:2:0 chroma).
        #[arg(long)]
        bitrate: Option<u32>,
        /// Drop the bitrate to a quarter for the middle third of the run and
        /// restore it, to exercise the live rate-control update.
        #[arg(long)]
        adapt: bool,
        /// Decode on a different tier to exercise mixed CPU/GPU streams.
        #[arg(long, value_parser = ["cpu", "gpu"])]
        decode: Option<String>,
        /// Delay each encoded packet before decoding, to model a slow server.
        #[arg(long, default_value_t = 0)]
        frame_delay_ms: u64,
    },
    /// Capture one frame of an output and write it as PNG
    Capture {
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value = "capture.png")]
        png: PathBuf,
        /// Also wait for a cursor shape event
        #[arg(long)]
        cursor: bool,
    },
    /// Move the pointer to the middle of an output and type text
    Input {
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value = "hello")]
        text: String,
        /// Click the left button at the centre before typing
        #[arg(long)]
        click: bool,
    },
    /// Capture one output frame and run it through the full Dual420 4:4:4
    /// pipeline: dmabuf import, GPU split, encode, decode, GPU recombine
    Pipeline {
        #[arg(long)]
        output: Option<String>,
    },
    /// Connect to a running `gliff-server --listen` and decode a few frames
    ServeTest {
        #[arg(long, default_value = "127.0.0.1:9000")]
        connect: String,
        #[arg(long, default_value_t = 30)]
        frames: usize,
    },
    /// Stream from a running `gliff-server --listen` for a while and report
    /// startup milestones, frame rate, interval jitter, latency and bandwidth
    StreamBench {
        #[arg(long, default_value = "127.0.0.1:9000")]
        connect: String,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// Ask the server for this stream size (0 = leave it alone).
        #[arg(long, default_value_t = 0)]
        width: u32,
        #[arg(long, default_value_t = 0)]
        height: u32,
        /// Ack frames without decoding them (server-side throughput only).
        #[arg(long)]
        no_decode: bool,
        /// Write one CSV row per frame here.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Print a per-second table of fps, kbit/s, keyframes and reconfigs.
        #[arg(long)]
        timeline: bool,
    },
    /// Watch or set the compositor's text clipboard (ext-data-control)
    Clipboard {
        /// Set the selection to this text and hold it, instead of watching.
        #[arg(long)]
        set: Option<String>,
        /// Seconds to run.
        #[arg(long, default_value_t = 3)]
        secs: u64,
    },
    /// Print the keysym on Caps Lock in each keymap the compositor serves
    /// its clients, which is the keymap of the keyboard used last
    Keymap {
        /// Seconds to watch.
        #[arg(long, default_value_t = 3)]
        secs: u64,
        /// Pass when a keymap seen maps Caps Lock to this keysym.
        #[arg(long)]
        caps: Option<String>,
    },
    /// Micro-benchmark the CPU colour/split reference the GPU shaders replace
    Bench {
        #[arg(long, default_value_t = 100)]
        iters: usize,
    },
    /// Run every non-interactive check
    All,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let target = Target {
        display: cli.display.clone(),
        instance: cli.instance.clone(),
    };
    let node = hypr_capture::render_node_for_target(cli.render_node.as_deref(), &target);
    let video = VideoMode::resolve(cli.video.as_deref());
    let capture_mode = CaptureMode::resolve(cli.capture.as_deref());
    match cli.cmd {
        Cmd::Protocols => protocols(&target)?,
        Cmd::Outputs => outputs(&target)?,
        Cmd::Permissions => permissions(&target)?,
        Cmd::Gpu => gpu_info(&node)?,
        Cmd::Surfaces { width, height } => surfaces(&node, width, height)?,
        Cmd::Roundtrip {
            width,
            height,
            frames,
            single,
            bitrate,
            adapt,
            decode,
            frame_delay_ms,
        } => roundtrip(
            &node,
            width,
            height,
            frames,
            !single,
            bitrate,
            adapt,
            video,
            decode.map(|v| {
                if v == "gpu" {
                    VideoMode::Gpu
                } else {
                    VideoMode::Cpu
                }
            }),
            frame_delay_ms,
        )?,
        Cmd::Capture {
            output,
            png,
            cursor,
        } => capture(&target, &node, capture_mode, output, &png, cursor)?,
        Cmd::Input {
            output,
            text,
            click,
        } => input(&target, output, &text, click)?,
        Cmd::Pipeline { output } => pipeline(&target, &node, capture_mode, output, video)?,
        Cmd::ServeTest { connect, frames } => serve_test(&node, &connect, frames, video)?,
        Cmd::StreamBench {
            connect,
            seconds,
            width,
            height,
            no_decode,
            csv,
            timeline,
        } => stream_bench(
            &node,
            &connect,
            seconds,
            (width, height),
            no_decode,
            csv.as_deref(),
            timeline,
            video,
        )?,
        Cmd::Clipboard { set, secs } => clipboard(&target, set, secs)?,
        Cmd::Keymap { secs, caps } => keymap(&target, secs, caps)?,
        Cmd::Bench { iters } => bench(iters)?,
        Cmd::All => {
            protocols(&target)?;
            outputs(&target)?;
            permissions(&target)?;
            let gpu_ok = match gpu_info(&node) {
                Ok(()) => true,
                Err(e) => {
                    status(
                        false,
                        &format!("GPU tier unavailable ({e}); the CPU pipeline will be used"),
                    );
                    false
                }
            };
            if gpu_ok && video == VideoMode::Gpu {
                if let Err(e) = roundtrip(
                    &node,
                    640,
                    360,
                    10,
                    true,
                    None,
                    false,
                    VideoMode::Gpu,
                    None,
                    0,
                ) {
                    status(
                        false,
                        &format!("GPU round trip failed ({e:#}); the CPU pipeline will be used"),
                    );
                }
            }
            roundtrip(
                &node,
                640,
                360,
                10,
                true,
                None,
                false,
                VideoMode::Cpu,
                None,
                0,
            )?;
        }
    }
    Ok(())
}

fn status(ok: bool, what: &str) {
    println!("{} {what}", if ok { "PASS" } else { "FAIL" });
}

fn protocols(target: &Target) -> Result<()> {
    struct S;
    impl Dispatch<WlRegistry, GlobalListContents> for S {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wayland_client::protocol::wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    let (_conn, globals, _queue) = hypr_wl::init::<S>(target)?;
    let list = hypr_wl::list_globals(&globals);
    for g in &list {
        println!("  {} v{}", g.interface, g.version);
    }
    let required: Vec<&str> = hypr_capture::REQUIRED_GLOBALS
        .iter()
        .chain(hypr_input::REQUIRED_GLOBALS)
        .copied()
        .collect();
    let mut all = true;
    for name in required {
        let ok = hypr_wl::has_global(&globals, name);
        all &= ok;
        status(ok, name);
    }
    // The ring needs one of these: dmabufs, or wl_shm where the compositor
    // offers no dmabuf (Hyper-V) or `--capture shm` asks for it.
    let dmabuf = hypr_wl::has_global(&globals, hypr_capture::BUFFER_GLOBALS[0]);
    let shm = hypr_wl::has_global(&globals, hypr_capture::BUFFER_GLOBALS[1]);
    all &= dmabuf || shm;
    status(
        dmabuf || shm,
        &format!(
            "capture buffers: {}",
            match (dmabuf, shm) {
                (true, true) => "zwp_linux_dmabuf_v1 (wl_shm as the fallback)",
                (true, false) => "zwp_linux_dmabuf_v1 only (no wl_shm fallback)",
                (false, true) => "wl_shm only (no zwp_linux_dmabuf_v1; CPU pipeline)",
                (false, false) => "neither zwp_linux_dmabuf_v1 nor wl_shm",
            }
        ),
    );
    let clipboard = hypr_wl::has_global(&globals, "ext_data_control_manager_v1")
        || hypr_wl::has_global(&globals, "zwlr_data_control_manager_v1");
    status(
        clipboard,
        "clipboard: ext_data_control_manager_v1 or zwlr_data_control_manager_v1",
    );
    status(
        hypr_wl::has_global(&globals, "zwp_keyboard_shortcuts_inhibit_manager_v1"),
        "zwp_keyboard_shortcuts_inhibit_manager_v1 (client side)",
    );
    if !all {
        bail!("required protocols missing");
    }
    Ok(())
}

fn outputs(target: &Target) -> Result<()> {
    for o in hypr_capture::list_outputs(target)? {
        let (lw, lh) = o.logical_size();
        println!(
            "  wl_output {} {}x{}@{}mHz scale {} logical {lw}x{lh} at {},{} ({})",
            o.name, o.width, o.height, o.refresh_mhz, o.scale, o.x, o.y, o.description
        );
    }
    match target.instance() {
        Ok(inst) => {
            for m in inst.monitors()? {
                println!(
                    "  hyprctl  {} {}x{}@{:.2} scale {} at {},{} focused={} disabled={}",
                    m.name,
                    m.width,
                    m.height,
                    m.refresh_rate,
                    m.scale,
                    m.x,
                    m.y,
                    m.focused,
                    m.disabled
                );
            }
        }
        Err(e) => println!("  hyprctl unavailable: {e}"),
    }
    Ok(())
}

fn permissions(target: &Target) -> Result<()> {
    let inst = target.instance()?;
    let enforce = inst.get_option("ecosystem:enforce_permissions")?;
    let on = enforce.as_bool().unwrap_or(false);
    status(!on, &format!("ecosystem:enforce_permissions = {on} (when on, add `permission = <gliff-server path>, screencopy, allow`)"));
    Ok(())
}

/// The colour bytes of a BGRA buffer, skipping the alpha/X byte whose captured
/// value is undefined.
fn rgb_channels(bgra: &[u8]) -> Vec<u8> {
    bgra.chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

fn pick_output(target: &Target, output: Option<String>) -> Result<String> {
    if let Some(o) = output {
        return Ok(o);
    }
    let list = hypr_capture::list_outputs(target)?;
    list.first()
        .map(|o| o.name.clone())
        .ok_or_else(|| anyhow!("no outputs"))
}

fn capture(
    target: &Target,
    node: &std::path::Path,
    mode: CaptureMode,
    output: Option<String>,
    png_path: &std::path::Path,
    cursor: bool,
) -> Result<()> {
    let output = pick_output(target, output)?;
    let mut cfg = CaptureConfig::new(output.clone());
    cfg.target = target.clone();
    cfg.render_node = node.to_path_buf();
    cfg.mode = mode;
    cfg.cursor = cursor;
    let (tx, rx) = mpsc::channel();
    let capturer = Capturer::start(
        cfg,
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    capturer.request_frame()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got_frame = false;
    let mut got_cursor = !cursor;
    while std::time::Instant::now() < deadline && !(got_frame && got_cursor) {
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        match ev {
            CaptureEvent::Ready {
                width,
                height,
                fourcc,
                modifier,
                ..
            } => {
                println!(
                    "  session ready {width}x{height} fourcc {:?} modifier {modifier:#x}",
                    fourcc.to_le_bytes().map(|b| b as char)
                );
            }
            CaptureEvent::Frame(frame) => {
                let image = frame.read_bgra()?;
                let (w, h) = (image.width as u32, image.height as u32);
                let rgb: Vec<u8> = image
                    .pixels
                    .chunks_exact(4)
                    .flat_map(|p| [p[2], p[1], p[0]])
                    .collect();
                write_png(png_path, w, h, &rgb)?;
                println!(
                    "  wrote {} ({w}x{h}, seq {}, damage {:?}, presentation {} ns)",
                    png_path.display(),
                    frame.sequence,
                    frame.damage,
                    frame.presentation_ns
                );
                got_frame = true;
            }
            CaptureEvent::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            } => {
                let opaque = argb.chunks(4).filter(|p| p[3] > 0).count();
                println!(
                    "  cursor shape {width}x{height} hotspot {hot_x},{hot_y} ({opaque} opaque px)"
                );
                got_cursor = true;
            }
            CaptureEvent::CursorPos { x, y, visible } => {
                println!("  cursor pos {x},{y} visible={visible}")
            }
            CaptureEvent::Stopped => bail!("capture stopped"),
            CaptureEvent::Error(e) => bail!("capture error: {e}"),
        }
    }
    status(got_frame, "captured a frame");
    if cursor {
        status(got_cursor, "received a cursor shape");
    }
    if !got_frame {
        bail!("no frame within 5 s");
    }
    Ok(())
}

fn write_png(path: &std::path::Path, w: u32, h: u32, rgb: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(rgb)?;
    Ok(())
}

fn input(target: &Target, output: Option<String>, text: &str, click: bool) -> Result<()> {
    let output = pick_output(target, output)?;
    let info = hypr_capture::list_outputs(target)?
        .into_iter()
        .find(|o| o.name == output)
        .ok_or_else(|| anyhow!("no output {output}"))?;
    let mut cfg = InputConfig::new(output.clone());
    cfg.target = target.clone();
    let (tx, rx) = mpsc::channel();
    let input = Input::start(
        cfg,
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    if let Ok(InputEvent::Error(e)) = rx.recv_timeout(Duration::from_millis(100)) {
        bail!("input error: {e}");
    }
    let (lw, lh) = info.logical_size();
    input.motion(lw as f64 / 2.0, lh as f64 / 2.0)?;
    std::thread::sleep(Duration::from_millis(50));
    if click {
        input.button(keys::BTN_LEFT, true)?;
        std::thread::sleep(Duration::from_millis(30));
        input.button(keys::BTN_LEFT, false)?;
        std::thread::sleep(Duration::from_millis(100));
    }
    for ch in text.chars() {
        let code = match ch {
            'h' => keys::KEY_H,
            'e' => keys::KEY_E,
            'l' => keys::KEY_L,
            'o' => keys::KEY_O,
            ' ' => keys::KEY_SPACE,
            '\n' => keys::KEY_ENTER,
            other => bail!("no keycode for {other:?} in the smoke test"),
        };
        input.key(code, true)?;
        std::thread::sleep(Duration::from_millis(25));
        input.key(code, false)?;
        std::thread::sleep(Duration::from_millis(25));
    }
    input.release_all()?;
    std::thread::sleep(Duration::from_millis(50));
    status(
        true,
        &format!(
            "moved pointer to {},{} on {output} and typed {text:?}",
            lw / 2,
            lh / 2
        ),
    );
    Ok(())
}

fn serve_test(node: &std::path::Path, addr: &str, frames: usize, video: VideoMode) -> Result<()> {
    use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg};
    use gliff_transport::Framed;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let stream = tokio::net::TcpStream::connect(addr).await.with_context(|| format!("connect {addr}"))?;
        stream.set_nodelay(true)?;
        let (rd, wr) = tokio::io::split(stream);
        let mut reader = Framed::new(rd);
        let mut writer = Framed::new(wr);
        let caps = ClientCaps { codecs: vec![Codec::H264], max_width: 1280, max_height: 720, chroma: vec![ChromaMode::Dual420, ChromaMode::Single420], features: gliff_proto::features() };
        writer.write_msg(&ClientMsg::Hello { version: gliff_proto::PROTOCOL_VERSION, keymap: String::new(), caps }).await?;
        let ack = reader.read_msg::<ServerMsg>().await?;
        let ServerMsg::HelloAck { session, outputs, .. } = ack else { bail!("expected HelloAck, got {ack:?}") };
        eprintln!("  HelloAck: headless={} output={} ({} outputs)", session.headless, session.output, outputs.len());
        // A Ping may arrive before the StreamConfig; answer it right away.
        let (mut w, mut h, mut chroma) = loop {
            match reader.read_msg::<ServerMsg>().await? {
                ServerMsg::Ping { t } => writer.write_msg(&ClientMsg::Pong { t }).await?,
                ServerMsg::StreamConfig { width, height, chroma, .. } => break (width as usize, height as usize, chroma),
                o => bail!("expected StreamConfig, got {o:?}"),
            }
        };
        eprintln!("  StreamConfig: {w}x{h} chroma {chroma:?}");
        let gpu = match video {
            VideoMode::Gpu => Some(Gpu::open(Some(node))?),
            VideoMode::Cpu => None,
        };
        let mut decoder = serve_decoder(&gpu, chroma != ChromaMode::Single420, w as u32, h as u32)?;
        let mut got = 0usize;
        let mut keyframes = 0usize;
        let mut scaled = false;
        // GLIFF_SEND_CLIP offers text; GLIFF_SEND_CLIP_FILE offers a file as
        // application/octet-stream; GLIFF_SEND_CLIP_FILES (colon-separated
        // paths) offers copied files. GLIFF_RECV_CLIP_FILE stores a received
        // octet-stream and GLIFF_RECV_CLIP_DIR the received files. Text
        // received is printed as CLIP-RECV.
        use gliff_proto::clipboard::{is_text_mime, safe_relative_path, CHUNK, TEXT_MIME, TEXT_MIMES, WINDOW};
        use gliff_proto::ClipboardItem;
        const OCTET: &str = "application/octet-stream";
        let send_clip = std::env::var("GLIFF_SEND_CLIP").ok();
        let send_file = std::env::var("GLIFF_SEND_CLIP_FILE").ok().map(|p| std::fs::read(&p).with_context(|| format!("read {p}"))).transpose()?;
        let send_files = match std::env::var("GLIFF_SEND_CLIP_FILES") {
            Ok(list) => gliff_transport::clipboard::files::list_files(&list.split(':').map(PathBuf::from).collect::<Vec<_>>())?,
            Err(_) => Default::default(),
        };
        // GLIFF_SEND_KEYMAP_OPTIONS sends a us keymap with these xkb options
        // after the first frames, then taps Shift so the compositor makes it
        // the active keymap.
        let mut send_keymap = std::env::var("GLIFF_SEND_KEYMAP_OPTIONS").ok().map(|options| {
            hypr_input::keymap_from_names(&hypr_input::KeymapNames { layout: "us".into(), options: Some(options), ..Default::default() })
        }).transpose()?;
        // GLIFF_SEND_KEYMAP_SWITCH types evdev key 16 and Return twice, each
        // right behind a keymap change: first under us (Q, so "q"), then under
        // fr (A, so "a"). A remote that keeps the first keymap types "q" twice.
        let mut keymap_switch = if std::env::var_os("GLIFF_SEND_KEYMAP_SWITCH").is_some() {
            ["us", "fr"].iter().map(|layout| hypr_input::keymap_from_names(&hypr_input::KeymapNames { layout: (*layout).into(), ..Default::default() })).collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let recv_file = std::env::var("GLIFF_RECV_CLIP_FILE").ok();
        let recv_dir = std::env::var("GLIFF_RECV_CLIP_DIR").ok().map(PathBuf::from);
        let mut clip_recv: Vec<u8> = Vec::new();
        let mut recv_mime = String::new();
        // The server's offered files and the index of the one being fetched.
        let mut recv_files: Vec<gliff_proto::ClipboardFile> = Vec::new();
        let mut recv_file_idx: Option<usize> = None;
        // Serial of the server's current offer, echoed in every request.
        let mut recv_serial = 0u32;
        // Outgoing transfer: (id, bytes, next offset); at most WINDOW chunks unacked.
        let mut serving: Option<(u32, Vec<u8>, usize, u32)> = None;
        async fn push_chunks<W: tokio::io::AsyncWrite + Unpin>(writer: &mut Framed<W>, serving: &mut Option<(u32, Vec<u8>, usize, u32)>) -> Result<()> {
            let Some((id, bytes, offset, unacked)) = serving.as_mut() else { return Ok(()) };
            while *unacked < WINDOW {
                let end = (*offset + CHUNK).min(bytes.len());
                let done = end == bytes.len();
                writer.write_msg_with_payloads(&ClientMsg::ClipboardData { id: *id, offset: *offset as u64, data_len: (end - *offset) as u32, done }, &[&bytes[*offset..end]]).await?;
                *offset = end;
                *unacked += 1;
                if done { *serving = None; return Ok(()); }
            }
            Ok(())
        }
        fn spool_path(dir: &std::path::Path, f: &gliff_proto::ClipboardFile) -> Result<PathBuf> {
            safe_relative_path(&f.path).map(|r| dir.join(r)).context("unsafe path")
        }
        /// Create directory entries from `from` on and request the next file; None when all are stored.
        async fn request_next_file<W: tokio::io::AsyncWrite + Unpin>(writer: &mut Framed<W>, files: &[gliff_proto::ClipboardFile], serial: u32, dir: &std::path::Path, from: usize) -> Result<Option<usize>> {
            for (i, f) in files.iter().enumerate().skip(from) {
                let path = spool_path(dir, f)?;
                if f.dir { std::fs::create_dir_all(&path)?; continue; }
                if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
                writer.write_msg(&ClientMsg::ClipboardRequest { id: 3 + 2 * i as u32, serial, item: ClipboardItem::File(i as u32) }).await?;
                return Ok(Some(i));
            }
            eprintln!("CLIP-RECV-FILES: {} entries", files.len());
            Ok(None)
        }
        while got < frames {
            let msg = reader.read_msg::<ServerMsg>().await?;
            match msg {
                ServerMsg::VideoFrame { frame_id, keyframe, data_len, aux_len, .. } => {
                    let main = reader.read_payload(data_len).await?;
                    let aux = if aux_len > 0 { reader.read_payload(aux_len).await?.to_vec() } else { Vec::new() };
                    if keyframe { keyframes += 1; }
                    let out = decoder.decode_to_bgra(&main, &aux)?;
                    if out.is_some() { got += 1; }
                    if got == 1 { eprintln!("  first decoded frame ok ({}x{}, main {} aux {} bytes, key {keyframe})", w, h, data_len, aux_len); }
                    writer.write_msg(&ClientMsg::FrameAck { frame_id, decoded_at_ms: 0 }).await?;
                    if got == 2 {
                        if let Some(keymap) = send_keymap.take() {
                            const KEY_LEFTSHIFT: u32 = 42;
                            writer.write_msg(&ClientMsg::Keymap { keymap }).await?;
                            writer.write_msg(&ClientMsg::Key { keycode: KEY_LEFTSHIFT, pressed: true }).await?;
                            writer.write_msg(&ClientMsg::Key { keycode: KEY_LEFTSHIFT, pressed: false }).await?;
                        }
                    }
                    if (got == 4 || got == 6) && !keymap_switch.is_empty() {
                        const KEY_Q: u32 = 16;
                        const KEY_ENTER: u32 = 28;
                        writer.write_msg(&ClientMsg::Keymap { keymap: keymap_switch.remove(0) }).await?;
                        for keycode in [KEY_Q, KEY_ENTER] {
                            writer.write_msg(&ClientMsg::Key { keycode, pressed: true }).await?;
                            writer.write_msg(&ClientMsg::Key { keycode, pressed: false }).await?;
                        }
                    }
                    if got == 3 { writer.write_msg(&ClientMsg::Resize { width: 800, height: 600, scale: 2.0 }).await?; }
                    if got == 4 && (send_clip.is_some() || send_file.is_some() || !send_files.entries.is_empty()) {
                        let mut mime_types: Vec<String> = Vec::new();
                        if send_clip.is_some() { mime_types.extend(TEXT_MIMES.iter().map(|m| m.to_string())); }
                        if send_file.is_some() { mime_types.push(OCTET.into()); }
                        writer.write_msg(&ClientMsg::ClipboardOffer { serial: 1, mime_types, files: send_files.entries.clone() }).await?;
                    }
                }
                ServerMsg::StreamConfig { width, height, chroma: c, scale_milli, .. } => {
                    // A pace-only reconfig comes without a keyframe; reset
                    // the decoder only when the coded stream changes.
                    if (width as usize, height as usize, c) != (w, h, chroma) {
                        decoder = serve_decoder(&gpu, c != ChromaMode::Single420, width, height)?;
                    }
                    w = width as usize; h = height as usize; chroma = c;
                    eprintln!("  reconfig to {w}x{h} scale {scale_milli}");
                    scaled = if session.headless { scale_milli == 2000 } else { w <= 800 && h <= 600 && scale_milli < 1000 };
                }
                ServerMsg::CursorShape { argb_len, .. } => { let _ = reader.read_payload(argb_len).await?; }
                // The server offers its selection; ask for one item and keep it once complete.
                ServerMsg::ClipboardOffer { serial, mime_types, files } => {
                    recv_serial = serial;
                    eprintln!("  server offers {mime_types:?} and {} file entries", files.len());
                    let want = if recv_file.is_some() && mime_types.iter().any(|m| m == OCTET) { Some(OCTET) }
                        else if mime_types.iter().any(|m| is_text_mime(m)) { Some(TEXT_MIME) } else { None };
                    if let (Some(dir), false) = (&recv_dir, files.is_empty()) {
                        clip_recv.clear();
                        recv_files = files;
                        recv_file_idx = request_next_file(&mut writer, &recv_files, recv_serial, dir, 0).await?;
                    } else if let Some(mime) = want {
                        clip_recv.clear();
                        recv_mime = mime.to_string();
                        writer.write_msg(&ClientMsg::ClipboardRequest { id: 1, serial: recv_serial, item: ClipboardItem::Mime(mime.into()) }).await?;
                    }
                }
                ServerMsg::ClipboardData { id, data_len, done, .. } => {
                    let bytes = reader.read_payload(data_len).await?;
                    clip_recv.extend_from_slice(&bytes);
                    if done {
                        let data = std::mem::take(&mut clip_recv);
                        if let (Some(i), Some(dir)) = (recv_file_idx, &recv_dir) {
                            std::fs::write(spool_path(dir, &recv_files[i])?, &data)?;
                            recv_file_idx = request_next_file(&mut writer, &recv_files, recv_serial, dir, i + 1).await?;
                        } else if recv_mime == OCTET {
                            let path = recv_file.clone().unwrap_or_default();
                            std::fs::write(&path, &data).with_context(|| format!("write {path}"))?;
                            eprintln!("CLIP-RECV-FILE: {} bytes", data.len());
                        } else if let Ok(t) = String::from_utf8(data) { eprintln!("CLIP-RECV: {t}"); }
                    } else {
                        writer.write_msg(&ClientMsg::ClipboardAck { id, received: clip_recv.len() as u64 }).await?;
                    }
                }
                // The server pastes something we offered: stream it within the window.
                ServerMsg::ClipboardRequest { id, serial: _, item } => {
                    let bytes = match &item {
                        ClipboardItem::Mime(m) if m == OCTET => send_file.clone().unwrap_or_default(),
                        ClipboardItem::Mime(m) if is_text_mime(m) => send_clip.clone().unwrap_or_default().into_bytes(),
                        ClipboardItem::File(_) if send_files.path_for(&item).is_some() => std::fs::read(send_files.path_for(&item).unwrap())?,
                        _ => { writer.write_msg(&ClientMsg::ClipboardAbort { id }).await?; continue; }
                    };
                    eprintln!("  serving {} bytes for {item:?}", bytes.len());
                    serving = Some((id, bytes, 0, 0));
                    push_chunks(&mut writer, &mut serving).await?;
                }
                ServerMsg::ClipboardAck { .. } => {
                    if let Some(s) = serving.as_mut() { s.3 = s.3.saturating_sub(1); }
                    push_chunks(&mut writer, &mut serving).await?;
                }
                ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. } | ServerMsg::Error { .. } => {}
                _ => {}
            }
        }
        writer.write_msg(&ClientMsg::Bye).await?;
        eprintln!("RESULT decoded {got} frames, {keyframes} keyframes");
        status(got >= frames && keyframes >= 1, &format!("decoded {got} frames from the server ({keyframes} keyframes, resize honoured)"));
        if frames > 3 {
            let what = if session.headless { "server applied the requested output scale" } else { "server scaled the mirrored screen down to the window" };
            status(scaled, what);
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn keymap(target: &Target, secs: u64, want_caps: Option<String>) -> Result<()> {
    const KEY_CAPSLOCK: u32 = 58;
    let (tx, rx) = std::sync::mpsc::channel();
    hypr_input::watch_keymap(
        target.clone(),
        Box::new(move |text| {
            let _ = tx.send(text);
        }),
    )?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok(text) = rx.recv_timeout(left) else {
            break;
        };
        let caps = hypr_input::key_name(&text, KEY_CAPSLOCK)?;
        println!("  keymap of {} bytes: Caps Lock is {caps}", text.len());
        seen.push(caps);
    }
    if let Some(want) = want_caps {
        status(
            seen.contains(&want),
            &format!("Caps Lock seen as {seen:?}, want {want}"),
        );
    }
    Ok(())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn stats(label: &str, unit: &str, mut v: Vec<f64>) {
    if v.is_empty() {
        println!("  {label:<18} (no samples)");
        return;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    println!(
        "  {label:<18} {unit}: p50 {:.1}  p95 {:.1}  max {:.1}",
        percentile(&v, 0.5),
        percentile(&v, 0.95),
        v[v.len() - 1]
    );
}

fn bench_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Stream for `seconds` and report startup milestones (from the moment the
/// TCP connect starts, no warmup exclusion), then steady-state numbers
/// (after a 1 s warmup). Latency uses the Ping/Pong clock offset, taken
/// from the pong with the smallest round trip.
#[allow(clippy::too_many_arguments)]
fn stream_bench(
    node: &std::path::Path,
    addr: &str,
    seconds: f64,
    size: (u32, u32),
    no_decode: bool,
    csv: Option<&std::path::Path>,
    timeline: bool,
    video: VideoMode,
) -> Result<()> {
    use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg};
    use gliff_transport::Framed;
    use std::io::Write;
    use std::time::Instant;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let start = Instant::now();
        let milestone = |name: &str| {
            println!("MILESTONE {name} {:.0} ms", start.elapsed().as_secs_f64() * 1000.0);
        };
        let stream = tokio::net::TcpStream::connect(addr).await.with_context(|| format!("connect {addr}"))?;
        stream.set_nodelay(true)?;
        milestone("connected");
        let (rd, wr) = tokio::io::split(stream);
        let mut reader = Framed::new(rd);
        let mut writer = Framed::new(wr);
        let caps = ClientCaps { codecs: vec![Codec::H264], max_width: 3840, max_height: 2160, chroma: vec![ChromaMode::Dual420, ChromaMode::Single420], features: gliff_proto::features() };
        writer.write_msg(&ClientMsg::Hello { version: gliff_proto::PROTOCOL_VERSION, keymap: String::new(), caps }).await?;
        let ack = reader.read_msg::<ServerMsg>().await?;
        let ServerMsg::HelloAck { session, .. } = ack else { bail!("expected HelloAck, got {ack:?}") };
        milestone("hello_ack");
        // A Ping may arrive before the StreamConfig; answer it right away
        // (it seeds the server's round-trip estimate).
        let (mut w, mut h, mut chroma) = loop {
            match reader.read_msg::<ServerMsg>().await? {
                ServerMsg::Ping { t } => writer.write_msg(&ClientMsg::Pong { t }).await?,
                ServerMsg::StreamConfig { width, height, chroma, .. } => break (width, height, chroma),
                o => bail!("expected StreamConfig, got {o:?}"),
            }
        };
        milestone("stream_config");
        println!("  connected: headless={} output={} stream {w}x{h} {chroma:?}", session.headless, session.output);
        if size.0 > 0 && size.1 > 0 {
            writer.write_msg(&ClientMsg::Resize { width: size.0, height: size.1, scale: 1.0 }).await?;
        }
        let gpu = match video {
            VideoMode::Gpu => Some(Gpu::open(Some(node))?),
            VideoMode::Cpu => None,
        };
        let mut decoder = if no_decode { None } else { Some(serve_decoder(&gpu, chroma != ChromaMode::Single420, w, h)?) };
        let mut csv_out = match csv { Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p)?)), None => None };
        if let Some(c) = csv_out.as_mut() { writeln!(c, "t_ms,frame_id,key,bytes,latency_ms,decode_ms")?; }

        // Reads happen on their own task, so pings go out while no frames
        // arrive.
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::task::spawn_local(async move {
            loop {
                let msg = match reader.read_msg::<ServerMsg>().await {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let (main, aux) = match &msg {
                    ServerMsg::VideoFrame { data_len, aux_len, .. } => {
                        let Ok(main) = reader.read_payload(*data_len).await else { break };
                        if *aux_len > 0 {
                            let Ok(aux) = reader.read_payload(*aux_len).await else { break };
                            (main, aux)
                        } else {
                            (main, bytes::Bytes::new())
                        }
                    }
                    ServerMsg::CursorShape { argb_len, .. } => {
                        let Ok(argb) = reader.read_payload(*argb_len).await else { break };
                        drop(argb);
                        (bytes::Bytes::new(), bytes::Bytes::new())
                    }
                    ServerMsg::ClipboardData { data_len, .. } => {
                        let Ok(d) = reader.read_payload(*data_len).await else { break };
                        drop(d);
                        (bytes::Bytes::new(), bytes::Bytes::new())
                    }
                    _ => (bytes::Bytes::new(), bytes::Bytes::new()),
                };
                if msg_tx.send((msg, main, aux)).is_err() {
                    break;
                }
            }
        });

        let warmup = Duration::from_secs_f64(1.0);
        let deadline = start + Duration::from_secs_f64(seconds);
        let mut ping_at = start;
        // Clock offset (server minus client) from the pong with the
        // smallest round trip; latency columns stay raw until one arrives.
        let mut offset_ms: Option<f64> = None;
        let mut best_ping_rtt = f64::MAX;
        let mut last_arrival: Option<Instant> = None;
        let mut intervals = Vec::new();
        let mut latency_recv = Vec::new();
        let mut latency_done = Vec::new();
        let mut decode_ms = Vec::new();
        let mut sizes = Vec::new();
        let (mut frames, mut keyframes, mut bytes) = (0u64, 0u64, 0u64);
        let mut reconfigs = 0u32;
        let mut first_frame = true;
        let mut first_decoded = true;
        let mut measured_from: Option<Instant> = None;
        // Per-second buckets: frames, bytes, keyframes, reconfigs.
        let mut buckets: Vec<[u64; 4]> = Vec::new();
        let mut bucket = |t: Instant, i: usize, n: u64| {
            let sec = t.duration_since(start).as_secs() as usize;
            if buckets.len() <= sec { buckets.resize(sec + 1, [0; 4]); }
            buckets[sec][i] += n;
        };
        loop {
            let now = Instant::now();
            if now >= deadline { break; }
            let (msg, main, aux) = tokio::select! {
                m = msg_rx.recv() => match m { Some(m) => m, None => break },
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(ping_at)) => {
                    writer.write_msg(&ClientMsg::Ping { t: bench_now_ms() }).await?;
                    ping_at += Duration::from_secs(1);
                    continue;
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => break,
            };
            match msg {
                ServerMsg::VideoFrame { frame_id, keyframe, pts_us, .. } => {
                    let arrived = Instant::now();
                    if first_frame { milestone("first_frame_received"); first_frame = false; }
                    let lat_recv = bench_now_ms() as f64 - offset_ms.unwrap_or(0.0) - pts_us as f64 / 1000.0;
                    let mut dec = 0.0;
                    let mut got_picture = true;
                    if let Some(d) = decoder.as_mut() {
                        let t0 = Instant::now();
                        got_picture = d.decode_to_bgra(&main, &aux)?.is_some();
                        dec = t0.elapsed().as_secs_f64() * 1000.0;
                    }
                    writer.write_msg(&ClientMsg::FrameAck { frame_id, decoded_at_ms: bench_now_ms() }).await?;
                    // Packet accounting is per arrival (the CPU decoder may
                    // hold a picture back one access unit); picture-derived
                    // numbers (latency, decode time) follow got_picture.
                    // The CPU tier's latency columns therefore lag by up to
                    // one frame interval.
                    let total = main.len() + aux.len();
                    bucket(arrived, 0, 1);
                    bucket(arrived, 1, total as u64);
                    if keyframe { bucket(arrived, 2, 1); }
                    if arrived.duration_since(start) >= warmup {
                        if measured_from.is_none() { measured_from = Some(arrived); }
                        frames += 1;
                        if keyframe { keyframes += 1; }
                        bytes += total as u64;
                        sizes.push(total as f64 / 1024.0);
                        if let Some(prev) = last_arrival { intervals.push(arrived.duration_since(prev).as_secs_f64() * 1000.0); }
                        if got_picture {
                            latency_recv.push(lat_recv);
                            latency_done.push(lat_recv + dec);
                            if decoder.is_some() { decode_ms.push(dec); }
                        }
                    }
                    last_arrival = Some(arrived);
                    if got_picture && first_decoded { milestone("first_frame_decoded"); first_decoded = false; }
                    if let Some(c) = csv_out.as_mut() {
                        writeln!(c, "{:.1},{frame_id},{},{total},{lat_recv:.2},{dec:.2}", arrived.duration_since(start).as_secs_f64() * 1000.0, keyframe as u8)?;
                    }
                }
                ServerMsg::StreamConfig { width, height, chroma: c, scale_milli, fps_cap, .. } => {
                    // A pace-only reconfig comes without a keyframe; reset
                    // the decoder only when the coded stream changes.
                    if (width, height, c) != (w, h, chroma) {
                        if decoder.is_some() { decoder = Some(serve_decoder(&gpu, c != ChromaMode::Single420, width, height)?); }
                        last_arrival = None;
                    }
                    w = width; h = height; chroma = c;
                    reconfigs += 1;
                    bucket(Instant::now(), 3, 1);
                    milestone("reconfig");
                    println!("  reconfig to {w}x{h} {chroma:?} scale {scale_milli} fps_cap {fps_cap}");
                }
                ServerMsg::Pong { t, server_now_ms } => {
                    let rtt = bench_now_ms().saturating_sub(t) as f64;
                    if rtt < best_ping_rtt {
                        best_ping_rtt = rtt;
                        offset_ms = Some(server_now_ms as f64 - (t as f64 + rtt / 2.0));
                    }
                }
                _ => {}
            }
        }
        // Measure to the end of the run, not the last arrival: a stream
        // that stalls near the end must not report inflated throughput.
        // Taken before the Bye write, whose blocking is not stream time.
        let end = Instant::now();
        writer.write_msg(&ClientMsg::Bye).await?;
        let span = measured_from.map(|t| end.duration_since(t).as_secs_f64()).unwrap_or(0.0).max(0.001);
        let fps = frames as f64 / span;
        let mbit = bytes as f64 * 8.0 / 1e6 / span;
        println!("  {w}x{h} {chroma:?}: {frames} frames in {span:.1} s after warmup ({keyframes} keyframes, {reconfigs} reconfigs)");
        println!("  fps {fps:.1}   {mbit:.1} Mbit/s   decode={}   clock offset {}", !no_decode, match offset_ms { Some(o) => format!("{o:.0} ms (ping rtt {best_ping_rtt:.0} ms)"), None => "unknown".into() });
        let stalls = intervals.iter().filter(|&&ms| ms > 100.0).count();
        stats("interval", "ms", intervals);
        println!("  {:<18} {stalls}", "gaps over 100 ms");
        stats("latency to recv", "ms", latency_recv);
        stats("latency decoded", "ms", latency_done);
        stats("decode", "ms", decode_ms);
        stats("frame size", "KiB", sizes);
        if timeline {
            println!("  timeline (per second): fps  kbit/s  keyframes  reconfigs");
            for (sec, b) in buckets.iter().enumerate() {
                println!("    t={sec:<3} {:>4} {:>8.0} {:>6} {:>6}", b[0], b[1] as f64 * 8.0 / 1000.0, b[2], b[3]);
            }
        }
        println!("RESULT fps={fps:.1} mbit={mbit:.1} stalls={stalls}");
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn clipboard(target: &Target, set: Option<String>, secs: u64) -> Result<()> {
    use gliff_proto::clipboard::{is_text_mime, TEXT_MIMES};
    use hypr_input::{Clipboard, ClipboardEvent};
    use std::io::{Read, Write};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let clip = Clipboard::start(
        target.clone(),
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    if let Some(text) = set {
        clip.offer(TEXT_MIMES.iter().map(|m| m.to_string()).collect());
        println!("  offering {text:?} as text; holding {secs}s");
        let mut pastes = 0;
        while std::time::Instant::now() < deadline {
            if let Ok(ClipboardEvent::Paste { mime_type, fd }) =
                rx.recv_timeout(std::time::Duration::from_millis(200))
            {
                let mut f = std::fs::File::from(fd);
                let _ = f.write_all(text.as_bytes());
                println!("  served a paste of {mime_type}");
                pastes += 1;
            }
        }
        status(true, &format!("clipboard offered ({pastes} pastes served)"));
    } else {
        println!("  watching selection for {secs}s");
        let mut got = false;
        while std::time::Instant::now() < deadline {
            if let Ok(ClipboardEvent::Selection { mime_types }) =
                rx.recv_timeout(std::time::Duration::from_millis(200))
            {
                println!("  selection offers {mime_types:?}");
                got = true;
                if let Some(m) = mime_types.iter().find(|m| is_text_mime(m)) {
                    let fd = clip.receive(m.clone())?;
                    let mut text = String::new();
                    std::fs::File::from(fd).read_to_string(&mut text)?;
                    println!("  text: {text:?}");
                }
            }
        }
        status(got, "observed a clipboard selection");
    }
    drop(clip);
    Ok(())
}

fn bench(iters: usize) -> Result<()> {
    use gliff_proto::chroma::{nv12_to_yuv444, recombine_yuv444, split_yuv444, yuv444_to_nv12};
    use gliff_sw::convert;
    use std::time::Instant;
    for (w, h) in [(1280usize, 720usize), (1920, 1080), (3840, 2160)] {
        // A representative BGRA frame.
        let mut bgra = vec![0u8; w * h * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            px[0] = (i & 0xff) as u8;
            px[1] = ((i >> 3) & 0xff) as u8;
            px[2] = ((i >> 6) & 0xff) as u8;
            px[3] = 255;
        }
        let time = |label: &str, n: usize, mut f: Box<dyn FnMut()>| {
            let t = Instant::now();
            for _ in 0..n {
                f();
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
            println!(
                "  {w}x{h} {label:<28} {ms:6.2} ms/frame  ({:.0} fps cap)",
                1000.0 / ms.max(0.001)
            );
        };
        let src = bgra_to_yuv444(&bgra, w * 4, w, h);
        let (m, a) = split_yuv444(&src);
        time("bgra->yuv444 (server)", iters, {
            let bgra = bgra.clone();
            Box::new(move || {
                let _ = bgra_to_yuv444(&bgra, w * 4, w, h);
            })
        });
        time("split 4:4:4->2xNV12 (server)", iters, {
            let src = src.clone();
            Box::new(move || {
                let _ = split_yuv444(&src);
            })
        });
        time("subsample 4:4:4->NV12 (single)", iters, {
            let src = src.clone();
            Box::new(move || {
                let _ = yuv444_to_nv12(&src);
            })
        });
        time("recombine 2xNV12->444 (client)", iters, {
            let m = m.clone();
            let a = a.clone();
            Box::new(move || {
                let _ = recombine_yuv444(&m, &a);
            })
        });
        time("upsample NV12->444 (single)", iters, {
            let m = m.clone();
            Box::new(move || {
                let _ = nv12_to_yuv444(&m);
            })
        });
        time("yuv444->bgra (client)", iters, {
            let src = src.clone();
            Box::new(move || {
                let _ = yuv444_to_bgra(&src);
            })
        });
        println!("  fused fixed-point kernels (gliff-sw):");
        time("bgra->i420 (server single)", iters, {
            let bgra = bgra.clone();
            Box::new(move || {
                let _ = convert::bgra_to_i420(&bgra, w * 4, w, h);
            })
        });
        time("bgra->444->2xi420 (server dual)", iters, {
            let bgra = bgra.clone();
            Box::new(move || {
                let _ = convert::split_yuv444(&convert::bgra_to_yuv444(&bgra, w * 4, w, h));
            })
        });
        let i420 = convert::bgra_to_i420(&bgra, w * 4, w, h);
        time("i420->bgra (client single)", iters, {
            let i420 = i420.clone();
            Box::new(move || {
                let _ = convert::i420_to_bgra(&i420.y, &i420.u, &i420.v, (w, w / 2, w / 2), w, h);
            })
        });
        time("yuv444->bgra (client dual)", iters, {
            let src = src.clone();
            Box::new(move || {
                let _ = convert::yuv444_to_bgra(&src);
            })
        });
    }
    Ok(())
}

fn gpu_info(node: &std::path::Path) -> Result<()> {
    let gpu = Gpu::open(Some(node))?;
    println!("  vulkan: {} ({})", gpu.name, gpu.driver);
    println!(
        "  va-api: {} (libva {}.{})",
        gpu.va.vendor, gpu.va.version.0, gpu.va.version.1
    );
    print_caps(&gpu.va_caps);
    print_encode(&gpu);
    match gpu.va_caps.can_decode() {
        Ok(()) => status(true, "VA-API H.264 decode"),
        Err(e) => status(false, &format!("VA-API H.264 decode: {e}")),
    }
    Ok(())
}

fn print_caps(caps: &gliff_va::Caps) {
    if let Some(e) = caps.encode_entrypoint {
        println!(
            "  H.264 encode: entrypoint {}, rate control {}, packed headers {:#x}, maximum {}x{}",
            gliff_va::display::entrypoint_name(e),
            gliff_va::display::rate_control_names(caps.rate_control).join("|"),
            caps.packed_headers,
            caps.max_width,
            caps.max_height
        );
    }
}

/// Which API encodes H.264 here, if either. The VA-API line keeps its own
/// PASS (`e2e.sh` keys the VA-API-only checks on it), but a VA-API driver
/// without an encoder is not a failure when Vulkan Video encodes instead:
/// that is the normal NVIDIA case, and a bare FAIL there reads as if the
/// GPU tier were unavailable.
fn print_encode(gpu: &Gpu) {
    let vk = gpu.vk_encode_info();
    let va = gpu.va_caps.can_encode();
    match (&va, &vk) {
        (Ok(()), _) => status(true, "VA-API H.264 encode"),
        (Err(e), Some(_)) => println!("  va-api H.264 encode: {e}; Vulkan Video encodes instead"),
        (Err(e), None) => status(false, &format!("VA-API H.264 encode: {e}")),
    }
    match (&vk, va.is_ok()) {
        (Some(info), false) => status(
            true,
            &format!(
                "Vulkan Video H.264 encode: maximum {}x{}, {} encode queue(s), {} {}",
                info.max_extent.0,
                info.max_extent.1,
                info.queues,
                info.std_header,
                info.std_header_version
            ),
        ),
        // The VA-API encoder is the one the pipeline picks; say the fallback
        // is there without a second PASS line for an encoder nothing uses.
        (Some(info), true) => println!(
            "  vulkan video: H.264 encode also available (maximum {}x{}), unused while VA-API encodes",
            info.max_extent.0, info.max_extent.1
        ),
        (None, false) => status(
            false,
            "Vulkan Video H.264 encode: no encoder (no VK_KHR_video_encode_h264 H.264 profile, \
             or GLIFF_VK_ENCODE=0); the CPU tier will be used",
        ),
        (None, true) => {}
    }
}

/// The hand-off: a driver-owned NV12 surface exported as a dmabuf, imported
/// into Vulkan, written by the split shader, and read back through the
/// driver to compare with the CPU split.
fn surfaces(node: &std::path::Path, width: u32, height: u32) -> Result<()> {
    use gliff_proto::chroma::yuv444_to_nv12;
    use gliff_va::{Surface, UsageHint};

    let gpu = Gpu::open(Some(node))?;
    let display = gpu.va.clone();
    let surface = Surface::new_nv12(&display, width, height, UsageHint::Encoder)?;
    let desc = surface.export()?;
    println!(
        "  surface {}x{} exported: modifier {:#x}, {} object(s) of {:?} bytes, planes {:?}",
        desc.width,
        desc.height,
        desc.modifier,
        desc.objects.len(),
        desc.object_sizes,
        desc.planes
            .iter()
            .map(|p| (p.object, p.offset, p.pitch))
            .collect::<Vec<_>>()
    );
    let (w, h) = (width as usize, height as usize);
    let bgra = synthetic_bgra(w, h, 0);
    let path = gliff_vk::split_into_surface(&gpu, &desc, &bgra, width, height)
        .context("split into the VA surface")?;
    println!("  split path: {path:?}");
    surface.sync()?;
    let (y, uv) = surface.read_nv12()?;
    let reference = yuv444_to_nv12(&bgra_to_yuv444(&bgra, w * 4, w, h));
    let max_diff = |a: &[u8], b: &[u8]| {
        a.iter()
            .zip(b)
            .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
            .max()
            .unwrap_or(0)
    };
    let (py, puv) = (psnr(&y, &reference.y), psnr(&uv, &reference.uv));
    println!(
        "  Y psnr {py:.1} dB (max diff {}), UV psnr {puv:.1} dB (max diff {})",
        max_diff(&y, &reference.y),
        max_diff(&uv, &reference.uv)
    );
    status(
        py > 45.0 && puv > 45.0,
        "Vulkan split shader output read back through VA-API",
    );
    Ok(())
}

/// A synthetic BGRA frame with sharp colour edges and motion, the case the
/// 4:4:4 path exists for.
fn synthetic_bgra(w: usize, h: usize, t: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let p = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
            let stripe = ((x + t * 3) / 32) % 4;
            let (b, g, r) = match stripe {
                0 => (255, 0, 0),
                1 => (0, 255, 0),
                2 => (0, 0, 255),
                _ => ((x * 255 / w) as u8, (y * 255 / h) as u8, 128),
            };
            p[0] = b;
            p[1] = g;
            p[2] = r;
            p[3] = 255;
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn roundtrip(
    node: &std::path::Path,
    width: u32,
    height: u32,
    frames: usize,
    dual: bool,
    bitrate: Option<u32>,
    adapt: bool,
    video: VideoMode,
    decode: Option<VideoMode>,
    frame_delay_ms: u64,
) -> Result<()> {
    let bitrate = bitrate.unwrap_or(4 * EncoderSettings::default_bitrate(width, height, 60));
    let settings = EncoderSettings {
        width,
        height,
        bitrate,
        framerate: 60,
        vbv_ms: EncoderSettings::DEFAULT_VBV_MS,
    };
    let decode = decode.unwrap_or(video);
    let gpu = if video == VideoMode::Gpu || decode == VideoMode::Gpu {
        Some(Gpu::open(Some(node))?)
    } else {
        None
    };
    println!("  encode={video:?} decode={decode:?} dual={dual} frame_delay_ms={frame_delay_ms}");
    let mut encoder = match video {
        VideoMode::Gpu => ProbeEncoder::Gpu(Box::new(Encoder::new(
            gpu.as_ref().unwrap(),
            settings,
            dual,
        )?)),
        VideoMode::Cpu => ProbeEncoder::Cpu(Box::new(gliff_sw::Encoder::new(
            sw_settings(width, height, bitrate),
            dual,
        )?)),
    };
    let mut decoder = match decode {
        VideoMode::Gpu => ProbeDecoder::Gpu(Box::new(Decoder::new(
            gpu.as_ref().unwrap(),
            dual,
            width,
            height,
        )?)),
        VideoMode::Cpu => ProbeDecoder::Cpu(Box::new(gliff_sw::Decoder::new(dual, width, height)?)),
    };
    let (w, h) = (width as usize, height as usize);
    let mut min_psnr = f64::MAX;
    let mut decoded = 0;
    let mut total_bytes = 0;
    // The CPU decoder outputs pictures in order but one access unit late, so
    // sources are queued and each output is compared with the oldest one.
    let mut pending: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();
    let start = std::time::Instant::now();
    for i in 0..frames {
        let src = synthetic_bgra(w, h, i);
        let force = i == frames / 2;
        if adapt && i == frames / 3 {
            println!("  bitrate -> {}", bitrate / 4);
            encoder.set_bitrate(bitrate / 4);
        }
        if adapt && i == 2 * frames / 3 {
            println!("  bitrate -> {bitrate}");
            encoder.set_bitrate(bitrate);
        }
        let t0 = std::time::Instant::now();
        let packet = encoder
            .encode_bgra(&src, force)
            .with_context(|| format!("encode frame {i}"))?;
        let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let aux = packet.aux.as_deref().unwrap_or(&[]);
        total_bytes += packet.main.len() + aux.len();
        if frame_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(frame_delay_ms));
        }
        let t1 = std::time::Instant::now();
        let out = decoder
            .decode_to_bgra(&packet.main, aux)
            .with_context(|| format!("decode frame {i}"))?;
        let dec_ms = t1.elapsed().as_secs_f64() * 1000.0;
        if i == 0 && !packet.keyframe {
            bail!("first packet is not a keyframe");
        }
        if force && !packet.keyframe {
            bail!("forced keyframe was not honoured");
        }
        pending.push_back(src);
        let Some(out) = out else {
            println!("  frame {i}: no output yet");
            continue;
        };
        let src = pending.pop_front().expect("a source per output");
        decoded += 1;
        let reference = reference_for(&src, w, h, dual);
        let p = psnr(&rgb_channels(&reference), &rgb_channels(&out));
        min_psnr = min_psnr.min(p);
        if let Some(dir) = std::env::var_os("GLIFF_VK_DUMP") {
            let dir = std::path::PathBuf::from(dir);
            let to_rgb = |b: &[u8]| -> Vec<u8> {
                b.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect()
            };
            write_png(
                &dir.join(format!("src{i}.png")),
                width,
                height,
                &to_rgb(&src),
            )?;
            write_png(
                &dir.join(format!("out{i}.png")),
                width,
                height,
                &to_rgb(&out),
            )?;
        }
        println!("  frame {i}: main {} aux {} bytes key={} enc {enc_ms:.2} ms dec {dec_ms:.2} ms rgb psnr {p:.1} dB", packet.main.len(), aux.len(), packet.keyframe);
    }
    if let (Some(out), Some(src)) = (decoder.flush()?, pending.pop_front()) {
        decoded += 1;
        let reference = reference_for(&src, w, h, dual);
        min_psnr = min_psnr.min(psnr(&rgb_channels(&reference), &rgb_channels(&out)));
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "  {frames} frames, {total_bytes} bytes, {:.1} fps end to end",
        frames as f64 / elapsed
    );
    status(
        decoded == frames,
        &format!("decoded {decoded}/{frames} frames"),
    );
    status(min_psnr > 30.0, &format!("min RGB PSNR {min_psnr:.1} dB"));
    if decoded != frames || min_psnr <= 30.0 {
        bail!("codec round-trip failed");
    }
    Ok(())
}

fn sw_settings(width: u32, height: u32, bitrate: u32) -> gliff_sw::EncoderSettings {
    gliff_sw::EncoderSettings {
        width,
        height,
        bitrate,
        framerate: 60,
    }
}

/// Either tier's encoder behind the shape the probe loops use.
enum ProbeEncoder {
    Gpu(Box<Encoder>),
    Cpu(Box<gliff_sw::Encoder>),
}

impl ProbeEncoder {
    fn encode_bgra(&mut self, bgra: &[u8], force_keyframe: bool) -> Result<EncodedFrame> {
        match self {
            Self::Gpu(e) => Ok(e.encode_bgra(bgra, force_keyframe)?),
            Self::Cpu(e) => {
                let f = e.encode_bgra(bgra, force_keyframe)?;
                Ok(EncodedFrame {
                    main: f.main,
                    aux: f.aux,
                    keyframe: f.keyframe,
                })
            }
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) {
        match self {
            Self::Gpu(e) => e.set_bitrate(bitrate),
            Self::Cpu(e) => e.set_bitrate(bitrate),
        }
    }
}

enum ProbeDecoder {
    Gpu(Box<Decoder>),
    Cpu(Box<gliff_sw::Decoder>),
}

fn serve_decoder(
    gpu: &Option<std::sync::Arc<Gpu>>,
    dual: bool,
    width: u32,
    height: u32,
) -> Result<ProbeDecoder> {
    Ok(match gpu {
        Some(gpu) => ProbeDecoder::Gpu(Box::new(Decoder::new(gpu, dual, width, height)?)),
        None => ProbeDecoder::Cpu(Box::new(gliff_sw::Decoder::new(dual, width, height)?)),
    })
}

impl ProbeDecoder {
    fn decode_to_bgra(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Gpu(d) => Ok(d.decode_to_bgra(main, aux)?),
            Self::Cpu(d) => Ok(d.decode(main, aux)?.map(|f| f.pixels)),
        }
    }

    /// Drain the picture the CPU decoder still buffers; the GPU decoder
    /// outputs every picture immediately and has nothing to drain.
    fn flush(&mut self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Gpu(_) => Ok(None),
            Self::Cpu(d) => Ok(d.flush()?.map(|f| f.pixels)),
        }
    }
}

/// The CPU reference result for `src` in the same chroma mode, so 4:2:0's
/// inherent loss is not counted against the codec under test.
fn reference_for(src: &[u8], w: usize, h: usize, dual: bool) -> Vec<u8> {
    let yuv = bgra_to_yuv444(src, w * 4, w, h);
    if dual {
        yuv444_to_bgra(&yuv)
    } else {
        yuv444_to_bgra(&gliff_proto::chroma::nv12_to_yuv444(
            &gliff_proto::chroma::yuv444_to_nv12(&yuv),
        ))
    }
}

/// Capture one frame and push it through the exact server and client
/// pipelines: dmabuf import, GPU split, two encodes, two decodes, GPU
/// recombine. Compares the result with the CPU 4:4:4 reference.
fn pipeline(
    target: &Target,
    node: &std::path::Path,
    mode: CaptureMode,
    output: Option<String>,
    video: VideoMode,
) -> Result<()> {
    if mode == CaptureMode::Shm && video == VideoMode::Gpu {
        bail!("--capture shm frames can only go through the CPU pipeline; add --video cpu");
    }
    let output = pick_output(target, output)?;
    let mut cfg = CaptureConfig::new(output.clone());
    cfg.target = target.clone();
    cfg.render_node = node.to_path_buf();
    cfg.mode = mode;
    // Capture the way the tier under test captures, as the server does: the
    // GPU tier imports the dmabuf as it is and must not get a wl_shm buffer,
    // while the CPU tier maps the frame and keeps the fallback.
    cfg.shm_fallback = video != VideoMode::Gpu;
    cfg.cursor = false;
    let (tx, rx) = mpsc::channel();
    let capturer = Capturer::start(
        cfg,
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    capturer.request_frame()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut captured = None;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(CaptureEvent::Frame(frame)) => {
                captured = Some(frame);
                break;
            }
            Ok(CaptureEvent::Error(e)) => bail!("capture error: {e}"),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    let frame = captured.ok_or_else(|| {
        anyhow!(
            "no frame captured (a headless output renders reliably; a physical KVM output may not)"
        )
    })?;
    if video == VideoMode::Gpu && frame.buffer.is_shm() {
        bail!(
            "the GPU tier needs dmabuf capture, but capture fell back to wl_shm; \
             this is what the server refuses too (run with --video cpu)"
        );
    }
    let info = &frame.buffer.info;
    // Even dimensions, as `read_bgra` crops to for the 4:2:0 path.
    let (w, h) = frame.extent();
    let fourcc = drm_fourcc::DrmFourcc::try_from(info.fourcc)
        .map_err(|_| anyhow!("captured fourcc {:#x} is not a DRM format", info.fourcc))?;
    println!(
        "  captured {w}x{h} {fourcc:?} modifier {:#x} {} from {output}",
        info.modifier,
        if frame.buffer.is_shm() {
            "(wl_shm)"
        } else {
            "(dmabuf)"
        }
    );
    // The CPU reference for the PSNR, when the frame maps at all: a tiled
    // capture buffer (NVIDIA, GPU tier) is not CPU-readable, which is why the
    // CPU tier captures into wl_shm instead.
    let reference = match frame.read_bgra() {
        Ok(r) => Some(r),
        Err(e) if video == VideoMode::Gpu => {
            println!("  no CPU reference for this frame: {e}; the PSNR is skipped");
            None
        }
        Err(e) => return Err(e).context("read the captured frame"),
    };
    let plane = DmabufPlane {
        fd: info.fd.as_fd(),
        width: info.width,
        height: info.height,
        offset: info.planes[0].offset,
        stride: info.planes[0].stride,
        fourcc,
        modifier: info.modifier,
    };

    let gpu = match video {
        VideoMode::Gpu => Some(Gpu::open(Some(node))?),
        VideoMode::Cpu => None,
    };
    let encoder_max = match &gpu {
        Some(gpu) => Encoder::max_size(gpu).context("encoder limits")?,
        None => gliff_sw::Encoder::MAX_SIZE,
    };
    let (sw, sh) = EncoderSettings::fit_extent(w, h, encoder_max);
    let scaled = (sw, sh) != (w, h);
    if scaled {
        println!(
            "  scaling {w}x{h} to {sw}x{sh} to fit the encoder maximum {}x{}",
            encoder_max.0, encoder_max.1
        );
    }
    let settings = EncoderSettings {
        width: sw,
        height: sh,
        bitrate: EncoderSettings::default_bitrate(sw, sh, 60),
        framerate: 60,
        vbv_ms: EncoderSettings::DEFAULT_VBV_MS,
    };
    let (mut encoder, mut decoder) = match &gpu {
        Some(gpu) => (
            ProbeEncoder::Gpu(Box::new(
                Encoder::new(gpu, settings, true).context("encoder")?,
            )),
            ProbeDecoder::Gpu(Box::new(
                Decoder::new(gpu, true, sw, sh).context("decoder")?,
            )),
        ),
        None => (
            ProbeEncoder::Cpu(Box::new(
                gliff_sw::Encoder::new(sw_settings(sw, sh, settings.bitrate), true)
                    .context("encoder")?,
            )),
            ProbeDecoder::Cpu(Box::new(
                gliff_sw::Decoder::new(true, sw, sh).context("decoder")?,
            )),
        ),
    };
    let t0 = std::time::Instant::now();
    let packet = match &mut encoder {
        ProbeEncoder::Gpu(enc) => enc
            .encode_dmabuf_transformed(1, &plane, frame.transform, true)
            .context("encode")?,
        ProbeEncoder::Cpu(enc) => {
            let reference = reference
                .as_ref()
                .ok_or_else(|| anyhow!("the CPU tier needs a mappable capture buffer"))?;
            let pixels = if scaled {
                downscale_bgra(
                    &reference.pixels,
                    reference.width,
                    reference.height,
                    sw as usize,
                    sh as usize,
                )
            } else {
                reference.pixels.clone()
            };
            let f = enc.encode_bgra(&pixels, true).context("encode")?;
            EncodedFrame {
                main: f.main,
                aux: f.aux,
                keyframe: f.keyframe,
            }
        }
    };
    let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let aux = packet.aux.as_deref().unwrap_or(&[]);
    println!(
        "  encoded main {} bytes, aux {} bytes, key={}, {enc_ms:.1} ms",
        packet.main.len(),
        aux.len(),
        packet.keyframe
    );
    let t1 = std::time::Instant::now();
    let out = match decoder
        .decode_to_bgra(&packet.main, aux)
        .context("decode")?
    {
        Some(out) => out,
        // The CPU decoder holds its only picture until the next unit or a
        // drain; this is the last unit, so drain.
        None => decoder
            .flush()
            .context("flush")?
            .ok_or_else(|| anyhow!("decode produced no frame"))?,
    };
    let dec_ms = t1.elapsed().as_secs_f64() * 1000.0;
    drop(frame);
    drop(capturer);

    let Some(reference) = reference else {
        println!("  decoded {dec_ms:.1} ms");
        status(
            true,
            "Dual420 4:4:4 GPU pipeline on a captured frame (ran; no PSNR without a \
             CPU-mappable capture buffer)",
        );
        return Ok(());
    };

    // What the CPU reference path makes of the same pixels, so only coding
    // loss and shader rounding count. A scaled stream is compared with a
    // CPU downscale, whose filter differs from the shader's, so the PSNR
    // is then informational and the check is that the round trip ran.
    let pixels = if scaled {
        downscale_bgra(
            &reference.pixels,
            reference.width,
            reference.height,
            sw as usize,
            sh as usize,
        )
    } else {
        reference.pixels.clone()
    };
    let cpu = yuv444_to_bgra(&bgra_to_yuv444(
        &pixels,
        sw as usize * 4,
        sw as usize,
        sh as usize,
    ));
    let rgb_psnr = psnr(&rgb_channels(&cpu), &rgb_channels(&out));
    println!("  decoded {dec_ms:.1} ms; end-to-end RGB PSNR vs CPU reference {rgb_psnr:.1} dB");
    // OpenH264 takes a first keyframe's QP from a fixed bits-per-pixel
    // table (QP 30 at the default bitrate), so the CPU tier scores a few dB
    // below the GPU on the same frame.
    let (tier, min_psnr) = if gpu.is_some() {
        ("GPU", 35.0)
    } else {
        ("CPU", 32.0)
    };
    let ok = scaled || rgb_psnr > min_psnr;
    status(
        ok,
        &format!("Dual420 4:4:4 {tier} pipeline on a captured frame"),
    );
    if !ok {
        bail!("pipeline PSNR too low");
    }
    Ok(())
}
