use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
#[cfg(any(target_os = "macos", test))]
use clap::Parser;
use gliff_proto::{
    features, is_incompatible, ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg,
    FEATURE_CURSOR_IN_VIDEO, PROTOCOL_VERSION,
};
#[cfg(target_os = "macos")]
use gliff_transport::spawn_ssh;
use gliff_transport::Framed;
#[cfg(any(target_os = "macos", test))]
use gliff_transport::SshTarget;
use serde_json::{json, Value};
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::sound::{Sound, MAX_AUDIO_PACKET};
use crate::{keys, native};

#[cfg(any(target_os = "macos", test))]
#[derive(Parser)]
#[command(name = "gliff", version, about = "Remote desktop over SSH")]
struct Cli {
    #[arg(long, hide = true)]
    host_bridge: Option<std::path::PathBuf>,
    host: Option<String>,
    /// List nearby Gliff computers as JSON without connecting or opening video.
    #[arg(long, conflicts_with_all = ["host", "connect", "check_frames"])]
    discover: bool,
    /// SSH port (default: SSH configuration, or the selected computer's advertised port).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    #[arg(long)]
    connect: Option<String>,
    #[arg(long, default_value = "gliff-server")]
    server_bin: String,
    #[arg(long, conflicts_with = "output")]
    headless: bool,
    #[arg(long)]
    output: Option<String>,
    #[arg(long, value_parser = ["cpu", "gpu"])]
    video: Option<String>,
    /// Do not share the clipboard. A choice saved for the machine in
    /// Settings still wins, as on Linux.
    #[arg(long)]
    no_clipboard: bool,
    /// Start with sound off, without saving that (Machine ▸ Play Sound
    /// turns it back on). Sound plays from hosts that send it (Linux).
    #[arg(long)]
    no_audio: bool,
    /// Decode a fixed number of frames without opening a window (integration test).
    #[arg(long)]
    check_frames: Option<u32>,
}

#[cfg(any(target_os = "macos", test))]
impl Cli {
    fn ssh_target(&self, selected: &crate::discovery::Selection) -> SshTarget {
        let mut target = SshTarget::new(selected.host.clone());
        target.server_bin = self.server_bin.clone();
        let port = self
            .port
            .or_else(|| self.host.is_none().then_some(selected.port));
        if let Some(port) = port {
            target.ssh_args.extend(["-p".into(), port.to_string()]);
        }
        target
    }
}

#[cfg(target_os = "macos")]
pub fn client_main() -> Result<()> {
    let cli = Rc::new(Cli::parse());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&rt, async move {
        if let Some(path) = &cli.host_bridge {
            return crate::host_bridge::app(path).await;
        }
        if cli.discover {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::discovery::list().await?)?
            );
            return Ok(());
        }
        let store = if cli.check_frames.is_some() {
            //= Integration runs neither read nor write the user's settings.
            gliff_settings::Store::in_memory()
        } else {
            gliff_settings::Store::open()
        };
        let shared = crate::settings::Shared::new(store);
        let cli_layer = crate::settings::cli_layer(
            cli.headless,
            cli.no_clipboard,
            &cli.server_bin,
            cli.video.as_deref(),
        );
        if cli.check_frames.is_some() {
            //= Integration run: one session, no window.
            let selected = cli
                .selection()
                .context("--check-frames needs a host or --connect")?;
            return visit(None, selected, cli.clone(), shared, cli_layer).await;
        }
        //= What to open: the command line's machine, else the tabs open when
        //= Gliff last quit, else the connection window's choice.
        let initial = match cli.selection() {
            Some(selected) => vec![selected],
            None => {
                //= Machines with auto_reconnect off stay closed until chosen.
                let restored: Vec<_> = shared
                    .open_tabs()
                    .into_iter()
                    .filter(|tab| {
                        let machine = crate::settings::machine_key(&tab.host);
                        let on = shared.resolve(&machine, &cli_layer).auto_reconnect;
                        if !on {
                            tracing::info!(%machine, "not reopening: auto_reconnect is off");
                        }
                        on
                    })
                    .collect();
                if !restored.is_empty() {
                    tracing::info!(tabs = restored.len(), "reopening the last tabs");
                    restored
                } else {
                    match crate::discovery::choose(cli.picker_screen(), &shared, &cli_layer).await?
                    {
                        Some(selected) => vec![selected],
                        None => return Ok(()),
                    }
                }
            }
        };
        //= One viewer process for every connection, so macOS can group
        //= their windows as tabs.
        let mut hub = Hub::open(&tab_title(&cli, &initial[0])).await?;
        let mut visits = Vec::new();
        for (i, selected) in initial.into_iter().enumerate() {
            let viewer = if i == 0 {
                hub.window(0)
            } else {
                hub.new_window(&tab_title(&cli, &selected)).await?
            };
            visits.push(spawn_visit(viewer, selected, &cli, &shared, &cli_layer));
        }
        //= The app's own events: the tab bar's + or File ▸ New Tab asks for
        //= another machine; quitting ends every session.
        while let Some(ev) = hub.app_event().await {
            match ev["type"].as_str() {
                Some("new_tab") => {
                    let chosen =
                        crate::discovery::choose(cli.picker_screen(), &shared, &cli_layer).await;
                    match chosen {
                        Ok(Some(selected)) => {
                            let viewer = hub.new_window(&tab_title(&cli, &selected)).await?;
                            visits.push(spawn_visit(viewer, selected, &cli, &shared, &cli_layer));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "connection window failed")
                        }
                    }
                }
                Some("open_settings") => {
                    let shared = shared.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(e) = crate::settings::window(shared).await {
                            tracing::warn!(error = %e, "settings window failed");
                        }
                    });
                }
                Some("bye") => break,
                _ => {}
            }
        }
        for visit in visits {
            let _ = visit.await;
        }
        Ok(())
    })
}

#[cfg(target_os = "macos")]
impl Cli {
    /// The machine the command line names, if any.
    fn selection(&self) -> Option<crate::discovery::Selection> {
        let screen = crate::discovery::ScreenMode::Mirror;
        match (&self.host, &self.connect) {
            (Some(h), _) => Some(crate::discovery::Selection {
                host: h.clone(),
                port: self.port.unwrap_or(22),
                screen,
            }),
            (None, Some(_)) => Some(crate::discovery::Selection {
                host: String::new(),
                port: 22,
                screen,
            }),
            (None, None) => None,
        }
    }

    /// The screen the connection window starts on, when a flag names one.
    fn picker_screen(&self) -> Option<crate::discovery::ScreenMode> {
        if self.headless {
            Some(crate::discovery::ScreenMode::Private)
        } else if self.output.is_some() {
            Some(crate::discovery::ScreenMode::Mirror)
        } else {
            None
        }
    }
}

/// A tab's title: the machine it shows.
#[cfg(target_os = "macos")]
fn tab_title(cli: &Cli, selected: &crate::discovery::Selection) -> String {
    match &cli.connect {
        Some(addr) if selected.host.is_empty() => format!("Gliff — {addr}"),
        _ => format!("Gliff — {}", selected.host),
    }
}

#[cfg(target_os = "macos")]
fn spawn_visit(
    viewer: Viewer,
    selected: crate::discovery::Selection,
    cli: &Rc<Cli>,
    shared: &crate::settings::SharedRef,
    cli_layer: &gliff_settings::Settings,
) -> tokio::task::JoinHandle<()> {
    let (cli, shared, cli_layer) = (cli.clone(), shared.clone(), cli_layer.clone());
    tokio::task::spawn_local(async move {
        let host = selected.host.clone();
        if let Err(e) = visit(Some(viewer), selected, cli, shared, cli_layer).await {
            tracing::warn!(%host, error = %format!("{e:#}"), "tab ended");
        }
    })
}

/// One machine for as long as its window (tab) is open: connect, follow
/// settings changes, and reconnect after failures. Without a window (an
/// integration run) it is one session.
#[cfg(target_os = "macos")]
async fn visit(
    mut viewer: Option<Viewer>,
    selected: crate::discovery::Selection,
    cli: Rc<Cli>,
    shared: crate::settings::SharedRef,
    cli_layer: gliff_settings::Settings,
) -> Result<()> {
    let host = selected.host.clone();
    ensure!(
        !host.is_empty() || cli.connect.is_some(),
        "enter an SSH address"
    );
    if cli.connect.is_none() {
        selected.validate()?;
    }
    let machine = crate::settings::machine_key(match &cli.connect {
        Some(addr) if host.is_empty() => addr,
        _ => &host,
    });
    if cli.check_frames.is_none() {
        shared.remember(&machine);
    }
    //= Windowed SSH sessions reopen at the next launch until their tab is
    //= closed (quitting Gliff keeps them).
    let restorable = viewer.is_some() && cli.connect.is_none();
    if restorable {
        shared.add_open_tab(&selected);
    }
    let screen_locked = cli.output.is_some();
    //= The sound switch for this visit: kept across reconnects, so a
    //= `--no-audio` run the user unmutes stays unmuted.
    let sound_wanted = Rc::new(Cell::new(!cli.no_audio && !shared.muted()));
    //= A failed or dropped session: retried automatically after one that
    //= connected, else the window waits for Reconnect (see `Retry`).
    let mut retry = Retry::default();
    //= The window already showed the error and the user closed it.
    let mut shown = false;
    //= One window for the whole visit: a settings change that needs a new
    //= session reconnects inside it.
    let result = loop {
        let opts = shared.resolve(&machine, &cli_layer);
        let established = Rc::new(Cell::new(false));
        let live = viewer.as_mut().map(|viewer| Live {
            //= `--video cpu` or a saved CPU decode keeps the CPU path.
            gpu: viewer.gpu && opts.decode != gliff_settings::Video::Cpu,
            viewer,
            shared: shared.clone(),
            machine: machine.clone(),
            cli: cli_layer.clone(),
            opts: opts.clone(),
            screen_locked,
            sound: sound_wanted.clone(),
            established: established.clone(),
        });
        let outcome = if let Some(addr) = &cli.connect {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    let (rd, wr) = tokio::io::split(stream);
                    run_session(rd, wr, &host, cli.check_frames, live).await
                }
                Err(e) => Err(e.into()),
            }
        } else {
            let mut target = cli.ssh_target(&selected);
            target.server_bin = opts.server_bin.clone();
            if cli.check_frames.is_none() {
                target.askpass = Some(native::helper_path()?);
            }
            if let Some(output) = &cli.output {
                target
                    .server_args
                    .extend(["--output".into(), output.clone()]);
            } else if opts.screen == gliff_settings::Screen::Own {
                target.server_args.push("--headless".into());
            }
            match spawn_ssh(&target) {
                Ok(mut ssh) => {
                    let r = run_session(ssh.stdout, ssh.stdin, &host, cli.check_frames, live).await;
                    let _ = ssh.child.kill().await;
                    r
                }
                Err(e) => Err(e.into()),
            }
        };
        match outcome {
            Ok(Outcome::Reconnect) => {
                tracing::info!(%machine, "settings changed; reconnecting");
                if let Some(v) = &viewer {
                    v.message("Reconnecting with the new settings…").await;
                }
            }
            Ok(Outcome::Done) => break Ok(()),
            Err(e) => {
                //= Integration runs fail at once; a window offers a reconnect.
                let Some(v) = viewer.as_mut() else {
                    break Err(e);
                };
                let incompatible = e.downcast_ref::<Incompatible>().is_some();
                let error = format!("{e:#}");
                //= Read now, so the Machine menu's switch applies to this failure.
                let auto = shared.resolve(&machine, &cli_layer).auto_reconnect;
                let (text, delay) = match retry.failed(established.get(), incompatible || !auto) {
                    Next::Retry { attempt } => {
                        tracing::warn!(%machine, %error, attempt, "session ended; reconnecting");
                        (
                            format!("{error}\nReconnecting… (attempt {attempt} of {MAX_RETRIES})"),
                            Some(RETRY_DELAY),
                        )
                    }
                    Next::Wait => {
                        tracing::warn!(%machine, %error, auto, "session ended; waiting for Reconnect");
                        let why = if auto {
                            ""
                        } else {
                            "Reconnect Automatically is off for this machine. "
                        };
                        (
                            format!("{error}\n{why}Reconnect, or close this window."),
                            None,
                        )
                    }
                };
                v.message_with_reconnect(&text).await;
                match v.wait_for_reconnect(delay, &shared, &machine).await {
                    Wait::Reconnect => continue,
                    Wait::Closed => {
                        shown = true;
                        break Err(e);
                    }
                }
            }
        }
    };
    if let Some(viewer) = viewer.take() {
        if restorable && viewer.events.closed {
            shared.remove_open_tab(&selected);
        }
        //= An error the window already showed needs no second showing.
        viewer.finish(if shown { &Ok(()) } else { &result }).await;
    }
    result
}

/// Automatic reconnects in a row before the window waits for the user, as on
/// Linux, and the pause before each.
const MAX_RETRIES: u32 = 5;
const RETRY_DELAY: Duration = Duration::from_millis(1500);

/// The server speaks another protocol: reconnecting cannot help, so it is
/// never retried automatically.
#[derive(Debug)]
struct Incompatible(String);

impl std::fmt::Display for Incompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "incompatible server: {}", self.0)
    }
}

impl std::error::Error for Incompatible {}

/// When a failed session is retried: the Linux client's rule (up to
/// `MAX_RETRIES`, never an incompatible server), with one difference. Only
/// a session that got through (its `HelloAck` arrived) starts automatic
/// retries, so a wrong password or a refused key does not open the SSH
/// prompt five times; the window waits for Reconnect instead.
#[derive(Debug, Default)]
struct Retry {
    /// Retries since the last session that got through.
    attempts: u32,
    /// A session got through since the window last waited for the user.
    auto: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Next {
    Retry { attempt: u32 },
    Wait,
}

impl Retry {
    /// `never`: this failure must wait for the user (an incompatible server,
    /// or the machine's auto_reconnect is off).
    fn failed(&mut self, established: bool, never: bool) -> Next {
        if established {
            self.attempts = 0;
            self.auto = true;
        }
        if self.auto && !never && self.attempts < MAX_RETRIES {
            self.attempts += 1;
            Next::Retry {
                attempt: self.attempts,
            }
        } else {
            self.auto = false;
            self.attempts = 0;
            Next::Wait
        }
    }
}

/// How waiting on a dead session's window ended.
#[derive(Debug, PartialEq, Eq)]
enum Wait {
    Reconnect,
    Closed,
}

/// How a session ended without an error.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The window closed or the integration run finished.
    Done,
    /// A setting changed that the session was started with.
    Reconnect,
}

/// One viewer window (a tab), kept across reconnects: the two halves of its
/// share of the bridge to the viewer helper (see [`Hub`]). Only
/// `client_main` (macOS) opens one; tests run sessions without a window.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Viewer {
    display: mpsc::Sender<(u8, Vec<u8>)>,
    events: Events,
    /// The helper can decode in hardware (VideoToolbox + Metal).
    gpu: bool,
}

/// The window's events, with any read ahead of time delivered first.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Events {
    //= Unbounded: the hub's router must never wait on one tab, or a tab
    //= stuck sending video would stall every other tab's events.
    rx: mpsc::UnboundedReceiver<Result<Value>>,
    pending: std::collections::VecDeque<Result<Value>>,
    /// The user closed this tab (`closed`), as opposed to quitting Gliff
    /// (`bye`): its machine is not reopened at the next launch.
    closed: bool,
    /// The window can be seen (the last `visible` event); kept across
    /// reconnects so a background tab stays silent.
    visible: bool,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Events {
    async fn recv(&mut self) -> Option<Result<Value>> {
        let ev = match self.pending.pop_front() {
            Some(ev) => Some(ev),
            None => self.rx.recv().await,
        };
        if let Some(Ok(v)) = &ev {
            if v["type"] == "closed" {
                self.closed = true;
            }
            if v["type"] == "visible" {
                self.visible = v["on"].as_bool() != Some(false);
            }
        }
        ev
    }
}

/// The one viewer helper process and its bridge, shared by every window so
/// that macOS can group them as tabs. Packets to the helper go to the window
/// named by the last `TARGET` packet (window 0 until one is sent); events
/// from it carry `"window": id`. Events without one belong to the app:
/// `new_tab`, `open_settings` and `video_caps` go to [`Hub::app_event`];
/// `keyboard_layout` goes to every window; `bye` (quit) to both.
#[cfg(target_os = "macos")]
struct Hub {
    _child: tokio::process::Child,
    out: mpsc::Sender<(u32, u8, Vec<u8>)>,
    routes: Routes,
    app: mpsc::UnboundedReceiver<Value>,
    /// Window 0's events, registered before the first event is read.
    first: Option<mpsc::UnboundedReceiver<Result<Value>>>,
    next: u32,
    gpu: bool,
}

#[cfg(target_os = "macos")]
type Routes =
    Rc<std::cell::RefCell<std::collections::HashMap<u32, mpsc::UnboundedSender<Result<Value>>>>>;

#[cfg(target_os = "macos")]
impl Hub {
    /// Start the helper; its first window (id 0) opens with `title`.
    async fn open(title: &str) -> Result<Self> {
        let mut child = native::spawn("app", &[title.to_string()])?;
        let mut rd = child.stdout.take().context("viewer stdout")?;
        let mut wr = child.stdin.take().context("viewer stdin")?;
        let (out, mut out_rx) = mpsc::channel::<(u32, u8, Vec<u8>)>(8);
        tokio::task::spawn_local(async move {
            let mut target = 0u32;
            while let Some((window, kind, bytes)) = out_rx.recv().await {
                if window != target {
                    native::write(&mut wr, native::TARGET, &window.to_le_bytes()).await?;
                    target = window;
                }
                native::write(&mut wr, kind, &bytes).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let routes: Routes = Default::default();
        let (first_tx, first) = mpsc::unbounded_channel::<Result<Value>>();
        routes.borrow_mut().insert(0, first_tx);
        let (app_tx, mut app) = mpsc::unbounded_channel::<Value>();
        let router = routes.clone();
        tokio::task::spawn_local(async move {
            loop {
                let ev = async {
                    let (kind, data) = native::read(&mut rd).await?;
                    ensure!(kind == native::EVENT, "unexpected UI packet");
                    Ok::<Value, anyhow::Error>(serde_json::from_slice(&data)?)
                }
                .await;
                let Ok(ev) = ev else {
                    //= The helper is gone: every window and the app see the end.
                    router.borrow_mut().clear();
                    break;
                };
                route_event(&router, &app_tx, ev);
            }
        });
        //= The helper's first event says whether it can decode in hardware.
        let mut gpu = false;
        match tokio::time::timeout(Duration::from_secs(5), app.recv()).await {
            Ok(Some(ev)) if ev["type"] == "video_caps" => {
                gpu = ev["hardware"].as_bool() == Some(true);
            }
            _ => tracing::warn!("the viewer did not report its video decode"),
        }
        tracing::info!(hardware = gpu, "viewer video decode");
        Ok(Self {
            _child: child,
            out,
            routes,
            app,
            first: Some(first),
            next: 1,
            gpu,
        })
    }

    /// The window `id` as a [`Viewer`]: window 0 is the one the helper opened
    /// at launch; others come from [`Hub::new_window`].
    fn window(&mut self, id: u32) -> Viewer {
        let rx = match (id, self.first.take()) {
            (0, Some(rx)) => rx,
            (_, first) => {
                self.first = first;
                let (tx, rx) = mpsc::unbounded_channel::<Result<Value>>();
                self.routes.borrow_mut().insert(id, tx);
                rx
            }
        };
        //= Each window keeps its own small queue (frames wait or are dropped
        //= per window, as before); one task merges them into the bridge.
        let (display, mut display_rx) = mpsc::channel::<(u8, Vec<u8>)>(2);
        let out = self.out.clone();
        tokio::task::spawn_local(async move {
            while let Some((kind, bytes)) = display_rx.recv().await {
                if out.send((id, kind, bytes)).await.is_err() {
                    break;
                }
            }
        });
        Viewer {
            display,
            events: Events {
                rx,
                pending: Default::default(),
                closed: false,
                visible: true,
            },
            gpu: self.gpu,
        }
    }

    /// Open another window; macOS adds it as a tab of the front window.
    async fn new_window(&mut self, title: &str) -> Result<Viewer> {
        let id = self.next;
        self.next += 1;
        let viewer = self.window(id);
        viewer
            .command(json!({ "type": "open_window", "title": title }))
            .await?;
        Ok(viewer)
    }

    /// The next app event; `None` once the helper is gone.
    async fn app_event(&mut self) -> Option<Value> {
        self.app.recv().await
    }
}

/// Where one event from the helper goes (see [`Hub`]). Never waits.
#[cfg(target_os = "macos")]
fn route_event(routes: &Routes, app: &mpsc::UnboundedSender<Value>, ev: Value) {
    let window = ev
        .get("window")
        .and_then(Value::as_u64)
        .and_then(|w| u32::try_from(w).ok());
    let kind = ev["type"].as_str().unwrap_or_default().to_string();
    match (window, kind.as_str()) {
        (Some(id), _) => {
            let mut routes = routes.borrow_mut();
            if let Some(tx) = routes.get(&id) {
                let _ = tx.send(Ok(ev));
            }
            if kind == "closed" {
                routes.remove(&id);
            }
        }
        (None, "keyboard_layout" | "bye") => {
            for tx in routes.borrow().values() {
                let _ = tx.send(Ok(ev.clone()));
            }
            if kind == "bye" {
                let _ = app.send(ev);
            }
        }
        (None, _) => {
            let _ = app.send(ev);
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Viewer {
    async fn command(&self, value: Value) -> Result<()> {
        self.display
            .send((native::EVENT, serde_json::to_vec(&value)?))
            .await
            .map_err(|_| anyhow::anyhow!("window closed"))
    }

    /// Replace the picture with a line of text.
    async fn message(&self, text: &str) {
        let _ = self.command(json!({ "message": text })).await;
    }

    /// Replace the picture with text and a Reconnect button.
    async fn message_with_reconnect(&self, text: &str) {
        let _ = self
            .command(json!({ "message": text, "reconnect": true }))
            .await;
    }

    /// After a session ended: wait for Reconnect (the button or Machine ▸
    /// Reconnect), the window closing, or `delay` for an automatic retry.
    /// Settings and the Machine menu keep working, so a wrong server command
    /// or screen can be fixed before reconnecting.
    async fn wait_for_reconnect(
        &mut self,
        delay: Option<Duration>,
        shared: &crate::settings::SharedRef,
        machine: &str,
    ) -> Wait {
        let timer = async {
            match delay {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(timer);
        loop {
            let ev = tokio::select! {
                _ = &mut timer => return Wait::Reconnect,
                ev = self.events.recv() => ev,
            };
            let Some(Ok(ev)) = ev else {
                return Wait::Closed;
            };
            match ev["type"].as_str() {
                Some("reconnect") => return Wait::Reconnect,
                Some("bye" | "closed") => return Wait::Closed,
                Some("open_settings") => {
                    let shared = shared.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(e) = crate::settings::window(shared).await {
                            tracing::warn!(error = %e, "settings window failed");
                        }
                    });
                }
                Some("setting") => {
                    if let Err(e) = crate::settings::apply_machine_setting(shared, machine, &ev) {
                        tracing::warn!(error = %e, "menu setting refused");
                    }
                }
                //= Input and resizes for a session that is gone.
                _ => {}
            }
        }
    }

    /// Show a failure and wait for the user to close the window.
    async fn finish(mut self, result: &Result<()>) {
        if let Err(error) = result {
            self.message(&format!("{error:#}\nClose this window and connect again."))
                .await;
            while let Some(Ok(event)) = self.events.recv().await {
                if event["type"] == "bye" || event["type"] == "closed" {
                    break;
                }
            }
        }
    }
}

/// What a windowed session needs beyond the connection: the window, and the
/// settings it was started with, to follow changes made while it runs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Live<'a> {
    viewer: &'a mut Viewer,
    /// Decode in the viewer (VideoToolbox + Metal) instead of on the CPU.
    gpu: bool,
    shared: crate::settings::SharedRef,
    machine: String,
    cli: gliff_settings::Settings,
    opts: gliff_settings::Resolved,
    screen_locked: bool,
    /// The Machine menu's sound switch, shared across reconnects.
    sound: Rc<Cell<bool>>,
    /// Set when the server's `HelloAck` arrives: the session got through.
    established: Rc<Cell<bool>>,
}

/// A session without a window (integration checks and tests).
#[cfg(test)]
async fn session<R, W>(rd: R, wr: W, host: &str, check: Option<u32>) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    run_session(rd, wr, host, check, None).await.map(|_| ())
}

async fn run_session<R, W>(
    rd: R,
    wr: W,
    host: &str,
    check: Option<u32>,
    live: Option<Live<'_>>,
) -> Result<Outcome>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);
    //= Hardware decode makes full colour cheap, so ask for it first.
    let mut gpu = live.as_ref().is_some_and(|l| l.gpu);
    //= Only a window plays sound; integration runs and tests do not offer it.
    let sound_cell = live.as_ref().map(|l| l.sound.clone());
    let established = live.as_ref().map(|l| l.established.clone());
    let mut sound = Sound::new(sound_cell.as_ref().is_some_and(|c| c.get()));
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: if check.is_none() {
                local_keymap().await?
            } else {
                String::new()
            },
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 3840,
                max_height: 2160,
                chroma: if gpu {
                    vec![ChromaMode::Dual420, ChromaMode::Single420]
                } else {
                    vec![ChromaMode::Single420, ChromaMode::Dual420]
                },
                features: features()
                    .into_iter()
                    .filter(|f| sound_cell.is_some() || f != gliff_proto::FEATURE_AUDIO)
                    .chain([FEATURE_CURSOR_IN_VIDEO.to_string()])
                    .collect(),
            },
        })
        .await?;
    //= Split the window into its parts so each select! branch borrows only
    //= what it uses. Without a window, frames go to a channel nobody reads.
    let visible = live.is_some();
    let (display_tx, _unread_display) = match &live {
        Some(l) => (l.viewer.display.clone(), None),
        None => {
            let (tx, rx) = mpsc::channel::<(u8, Vec<u8>)>(2);
            (tx, Some(rx))
        }
    };
    let (mut events, mut ctx) = match live {
        Some(Live {
            viewer,
            shared,
            machine,
            cli,
            opts,
            screen_locked,
            ..
        }) => (
            Some(&mut viewer.events),
            Some((shared, machine, cli, opts, screen_locked)),
        ),
        None => (None, None),
    };
    //= A tab already in the background starts silent.
    if let Some(events) = &events {
        let _ = sound.visible(events.visible);
    }
    let mut changes = ctx.as_ref().map(|(shared, ..)| shared.subscribe());
    let mut share_clipboard = ctx.as_ref().is_none_or(|(.., opts, _)| opts.clipboard);
    //= The saved switch as last seen, to notice a change from elsewhere.
    let mut last_muted = ctx.as_ref().is_some_and(|(shared, ..)| shared.muted());
    if let Some((_, _, _, opts, locked)) = &ctx {
        let mut state = crate::settings::machine_state(opts, *locked);
        sound.annotate(&mut state);
        display_tx
            .send((native::EVENT, serde_json::to_vec(&state)?))
            .await?;
        //= A reconnect needs the window's size again (a private screen
        //= follows it); a first session gets it from the window's start.
        display_tx
            .send((
                native::EVENT,
                serde_json::to_vec(&json!({"type": "report_size"}))?,
            ))
            .await?;
    }
    let (out, mut outgoing) = mpsc::channel::<(ClientMsg, Bytes)>(64);
    let mut writes = tokio::task::spawn_local(async move {
        while let Some((msg, payload)) = outgoing.recv().await {
            writer.write_msg_with_payloads(&msg, &[&payload]).await?;
        }
        Ok::<_, gliff_transport::Error>(())
    });
    let (incoming_tx, mut incoming) = mpsc::channel::<Result<(ServerMsg, Bytes)>>(8);
    let reads = tokio::task::spawn_local(async move {
        loop {
            let msg = async {
                let m = reader.read_msg::<ServerMsg>().await?;
                if let ServerMsg::AudioFrame { data_len, .. } = &m {
                    ensure!(
                        *data_len <= MAX_AUDIO_PACKET,
                        "audio packet of {data_len} bytes exceeds the limit"
                    );
                }
                let p = reader.read_payload(server_payload(&m)).await?;
                Ok((m, p))
            }
            .await;
            let stopped = msg.is_err();
            if incoming_tx.send(msg).await.is_err() || stopped {
                break;
            }
        }
    });
    let mut decoder = None;
    let mut coded_stream = None;
    let mut scale = 1000;
    let mut frames = 0;
    let mut got_hello = false;
    let mut idle = tokio::time::interval(Duration::from_millis(100));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_id = 0;
    //= Consecutive hardware decode failures; past a few, use the CPU.
    let mut gpu_errors = 0u32;
    let clipboard =
        crate::clipboard::Bridge::new(gliff_transport::clipboard::Side::Client, display_tx.clone());
    let mut clips = clipboard.take_outbound();
    let mut deadline = Box::pin(tokio::time::sleep(Duration::from_secs(20)));
    let result: Result<Outcome> = async {
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                // An idle Linux desktop sends no video after refinement.
                // Ask for a reply so the response timeout measures liveness.
                out.send((ClientMsg::Ping { t: now_ms() }, Bytes::new())).await?;
            }
            _ = &mut deadline => bail!("server stopped responding for 20 seconds"),
            result = &mut writes => { result??; bail!("network writer stopped"); }
            ev = async { events.as_mut().unwrap().recv().await }, if events.is_some() => {
                let ev = ev.context("window closed")??;
                if ev["type"] == "bye" || ev["type"] == "closed" { out.send((ClientMsg::Bye, Bytes::new())).await?; break; }
                if ev["type"] == "clipboard" { if share_clipboard { clipboard.local(&ev).await?; } }
                else if ev["type"] == "paste" { if share_clipboard { clipboard.paste(&ev); } }
                else if ev["type"] == "open_settings" {
                    if let Some((shared, ..)) = &ctx {
                        let shared = shared.clone();
                        tokio::task::spawn_local(async move {
                            if let Err(e) = crate::settings::window(shared).await {
                                tracing::warn!(error = %e, "settings window failed");
                            }
                        });
                    }
                }
                else if ev["type"] == "setting" {
                    //= The Machine menu; the change arrives back on `changes`.
                    if let Some((shared, machine, ..)) = &ctx {
                        if let Err(e) = crate::settings::apply_machine_setting(shared, machine, &ev) {
                            tracing::warn!(error = %e, "menu setting refused");
                        }
                    }
                }
                else if ev["type"] == "decoded" {
                    //= The viewer finished this frame (decode + recombine):
                    //= acknowledge it now, so the server paces on real decode time.
                    if let Some(frame_id) = ev["frame_id"].as_u64() {
                        gpu_errors = 0;
                        frames += 1;
                        out.send((ClientMsg::FrameAck { frame_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                    }
                }
                else if ev["type"] == "decode_error" || ev["type"] == "gpu_failed" {
                    let message = ev["message"].as_str().unwrap_or("unknown");
                    if !gpu {
                        //= Frames still in the pipe after the switch to the CPU:
                        //= acknowledge them; the keyframe is already requested.
                        if let Some(frame_id) = ev["frame_id"].as_u64() {
                            out.send((ClientMsg::FrameAck { frame_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                        }
                        continue;
                    }
                    gpu_errors += 1;
                    if ev["type"] == "gpu_failed" || gpu_errors >= 3 {
                        tracing::warn!(%message, "hardware decode failed; using the CPU");
                        gpu = false;
                        if let Some((width, height, chroma)) = coded_stream {
                            decoder = Some(gliff_sw::Decoder::new(chroma == ChromaMode::Dual420, width, height)?);
                        }
                    } else {
                        tracing::warn!(%message, "hardware decode error; requesting keyframe");
                    }
                    if let Some(frame_id) = ev["frame_id"].as_u64() {
                        out.send((ClientMsg::FrameAck { frame_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                    }
                    out.send((ClientMsg::RequestKeyframe, Bytes::new())).await?;
                }
                else if ev["type"] == "visible" {
                    //= Only a tab that can be seen plays its machine's sound.
                    if let Some(msg) = sound.visible(ev["on"].as_bool() != Some(false)) { out.send((msg, Bytes::new())).await?; }
                }
                else if ev["type"] == "sound" {
                    //= Machine ▸ Play Sound: apply now, then save it for both clients.
                    let on = ev["on"].as_bool().context("sound event without on")?;
                    if let Some(cell) = &sound_cell { cell.set(on); }
                    if let Some(msg) = sound.want(on) { out.send((msg, Bytes::new())).await?; }
                    if let Some((shared, _, _, opts, locked)) = &ctx {
                        last_muted = !on;
                        shared.set_muted(!on);
                        let mut state = crate::settings::machine_state(opts, *locked);
                        sound.annotate(&mut state);
                        display_tx.send((native::EVENT, serde_json::to_vec(&state)?)).await?;
                    }
                }
                else if ev["type"] == "reconnect" {
                    out.send((ClientMsg::Bye, Bytes::new())).await?;
                    return Ok(Outcome::Reconnect);
                }
                else if ev["type"] == "keyboard_layout" {
                    let keymap = crate::keymap::from_event(&ev)?;
                    out.send((ClientMsg::Keymap { keymap }, Bytes::new())).await?;
                }
                else if let Some(msg) = input(&ev) { out.send((msg, Bytes::new())).await?; }
            }
            Some((msg, payload)) = clips.recv() => { out.send((ClientMsg::from(msg), payload)).await?; }
            changed = async { changes.as_mut().unwrap().changed().await }, if changes.is_some() => {
                if changed.is_err() { changes = None; continue; }
                let Some((shared, machine, cli, opts, locked)) = &mut ctx else { continue };
                let new = shared.resolve(machine, cli);
                match crate::settings::change(opts, &new, *locked) {
                    crate::settings::Change::Reconnect => {
                        out.send((ClientMsg::Bye, Bytes::new())).await?;
                        return Ok(Outcome::Reconnect);
                    }
                    crate::settings::Change::Clipboard(on) => {
                        share_clipboard = on;
                        //= Turning sharing off withdraws what this Mac offered.
                        if !on { clipboard.local(&json!({})).await?; }
                        tracing::info!(share = on, "clipboard sharing changed");
                    }
                    crate::settings::Change::None => {}
                }
                *opts = new;
                //= The switch saved from elsewhere (the settings window, a hand edit).
                let muted = shared.muted();
                if muted != last_muted {
                    last_muted = muted;
                    if let Some(cell) = &sound_cell { cell.set(!muted); }
                    if let Some(msg) = sound.want(!muted) { out.send((msg, Bytes::new())).await?; }
                }
                let mut state = crate::settings::machine_state(opts, *locked);
                sound.annotate(&mut state);
                display_tx.send((native::EVENT, serde_json::to_vec(&state)?)).await?;
            }
            _ = idle.tick(), if decoder.is_some() => {
                let decoder: &mut gliff_sw::Decoder = decoder.as_mut().unwrap();
                if decoder.has_pending() {
                    if let Some(frame) = decoder.flush()? {
                        show(&display_tx, visible, frame, scale).await?;
                        frames += 1;
                        out.send((ClientMsg::FrameAck { frame_id: last_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                    }
                }
            }
            packet = incoming.recv() => {
                let (msg, payload) = packet.context("server closed")??;
                deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(20));
                match msg.into_clipboard() {
                    Ok(msg) => { if share_clipboard { clipboard.peer(msg, payload) } }
                    Err(msg) => match msg {
                        ServerMsg::HelloAck { version, session, features, .. } => {
                            if version != PROTOCOL_VERSION {
                                return Err(Incompatible(format!("server protocol {version}, this client {PROTOCOL_VERSION}")).into());
                            }
                            got_hello = true;
                            if let Some(e) = &established { e.set(true); }
                            if visible {
                                let command = json!({"type":"cursor_mode", "embedded":cursor_in_video(session.headless, &features)});
                                display_tx.send((native::EVENT, serde_json::to_vec(&command)?)).await?;
                            }
                            if let Some(msg) = sound.server_features(&features) { out.send((msg, Bytes::new())).await?; }
                            if let Some((_, _, _, opts, locked)) = &ctx {
                                //= The menu learns whether this server sends sound.
                                let mut state = crate::settings::machine_state(opts, *locked);
                                sound.annotate(&mut state);
                                display_tx.send((native::EVENT, serde_json::to_vec(&state)?)).await?;
                            }
                        }
                        ServerMsg::StreamConfig { codec, chroma, width, height, scale_milli, .. } => {
                            ensure!(got_hello && codec == Codec::H264 && width > 0 && height > 0 && scale_milli > 0, "unsupported stream configuration");
                            ensure!(matches!(chroma, ChromaMode::Single420 | ChromaMode::Dual420), "unsupported chroma mode");
                            // Pace, scale and pipeline metadata can change without
                            // a new coded stream or keyframe. Preserve references.
                            if coded_stream != Some((width, height, chroma)) {
                                coded_stream = Some((width, height, chroma));
                                if gpu {
                                    //= The viewer builds its decoders before this stream's frames.
                                    let config = json!({"type": "video_config", "width": width, "height": height,
                                        "dual": chroma == ChromaMode::Dual420, "scale_milli": scale_milli});
                                    display_tx.send((native::EVENT, serde_json::to_vec(&config)?)).await?;
                                    decoder = None;
                                } else {
                                    decoder = Some(gliff_sw::Decoder::new(chroma == ChromaMode::Dual420, width, height)?);
                                }
                            } else if gpu && scale != scale_milli {
                                let command = json!({"type": "video_scale", "scale_milli": scale_milli});
                                display_tx.send((native::EVENT, serde_json::to_vec(&command)?)).await?;
                            }
                            scale = scale_milli;
                            tracing::info!(width, height, scale_milli, ?chroma, gpu, "stream configured");
                        }
                        ServerMsg::VideoFrame { frame_id, data_len, aux_len, .. } if gpu => {
                            ensure!(payload.len() == data_len as usize + aux_len as usize, "invalid video payload");
                            ensure!(coded_stream.is_some(), "video before configuration");
                            last_id = frame_id;
                            //= Coded frames are never dropped (a lost P-frame corrupts
                            //= the next ones): wait for the window instead. The ack
                            //= follows its `decoded` event.
                            let packet = native::video_packet(frame_id, data_len, &payload)?;
                            display_tx.send((native::VIDEO, packet)).await?;
                        }
                        ServerMsg::VideoFrame { frame_id, data_len, aux_len, .. } => {
                            ensure!(payload.len() == data_len as usize + aux_len as usize, "invalid video payload");
                            let decoder = decoder.as_mut().context("video before configuration")?;
                            last_id = frame_id;
                            match decoder.decode(&payload[..data_len as usize], &payload[data_len as usize..]) {
                                Ok(Some(frame)) => { show(&display_tx, visible, frame, scale).await?; frames += 1; }
                                Ok(None) => {}
                                Err(e) => { tracing::warn!(error = %e, "decode failed; requesting keyframe"); out.send((ClientMsg::RequestKeyframe, Bytes::new())).await?; }
                            }
                            out.send((ClientMsg::FrameAck { frame_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                        }
                        ServerMsg::Ping { t } => { out.send((ClientMsg::Pong { t }, Bytes::new())).await?; }
                        ServerMsg::AudioStart { codec, sample_rate, channels } => sound.start(codec, sample_rate, channels),
                        ServerMsg::AudioFrame { seq, data_len } => {
                            ensure!(payload.len() == data_len as usize, "invalid audio payload");
                            sound.frame(seq, &payload);
                        }
                        ServerMsg::Error { code, message, .. } if is_incompatible(code) => return Err(Incompatible(message).into()),
                        ServerMsg::Error { message, .. } => bail!("server error: {message}"),
                        _ => {}
                    }
                }
            }
        }
        if check.is_some_and(|n| frames >= n) {
            out.send((ClientMsg::Bye, Bytes::new())).await?;
            println!("PASS: decoded {frames} frames from {host}");
            break;
        }
    }
    Ok(Outcome::Done)
    }.await;
    // Stop the transport immediately on failure, even while its error window
    // remains open. EOF tears down the remote host and releases held input.
    reads.abort();
    writes.abort();
    result
}

async fn show(
    tx: &mpsc::Sender<(u8, Vec<u8>)>,
    visible: bool,
    frame: gliff_sw::BgraFrame,
    scale: u32,
) -> Result<()> {
    ensure!(
        frame.width > 0
            && frame.height > 0
            && frame.pixels.len() == frame.width as usize * frame.height as usize * 4,
        "invalid decoded frame"
    );
    if visible {
        let mut pixels = Vec::with_capacity(12 + frame.pixels.len());
        for n in [frame.width, frame.height, scale] {
            pixels.extend_from_slice(&n.to_le_bytes());
        }
        pixels.extend_from_slice(&frame.pixels);
        // A slow display must not stop network reads or accumulate stale frames.
        match tx.try_send((native::PIXELS, pixels)) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("window closed"),
        }
    }
    Ok(())
}

fn input(ev: &Value) -> Option<ClientMsg> {
    match ev["type"].as_str()? {
        "key" => Some(ClientMsg::Key {
            keycode: keys::evdev(u16::try_from(ev["keycode"].as_u64()?).ok()?)?,
            pressed: ev["pressed"].as_bool()?,
        }),
        "motion" => Some(ClientMsg::PointerMotion {
            x: ev["x"].as_f64()?,
            y: ev["y"].as_f64()?,
        }),
        "button" => Some(ClientMsg::PointerButton {
            button: match ev["button"].as_u64()? {
                0 => 272,
                1 => 273,
                2 => 274,
                _ => return None,
            },
            pressed: ev["pressed"].as_bool()?,
        }),
        "resize" => {
            let width = u32::try_from(ev["width"].as_u64()?).ok()?;
            let height = u32::try_from(ev["height"].as_u64()?).ok()?;
            let scale = ev["scale"].as_f64()?;
            if width == 0 || height == 0 || !scale.is_finite() || scale <= 0.0 {
                return None;
            }
            // Cap Retina viewports proportionally: independent width/height
            // clamps change the remote aspect ratio and create letterboxing.
            let ratio = (3840.0 / f64::from(width))
                .min(2160.0 / f64::from(height))
                .min(1.0);
            Some(ClientMsg::Resize {
                width: (f64::from(width) * ratio).round().max(2.0) as u32,
                height: (f64::from(height) * ratio).round().max(2.0) as u32,
                scale: (scale * ratio) as f32,
            })
        }
        "axis" => Some(ClientMsg::PointerAxis {
            axis: if ev["horizontal"].as_bool()? {
                gliff_proto::Axis::Horizontal
            } else {
                gliff_proto::Axis::Vertical
            },
            value: ev["value"].as_f64()?,
            discrete: None,
            stop: false,
        }),
        _ => None,
    }
}

async fn local_keymap() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let mut child = native::spawn("keyboard-layout", &[])?;
        let result = async {
            let (kind, bytes) = tokio::time::timeout(
                Duration::from_secs(5),
                native::read(child.stdout.as_mut().unwrap()),
            )
            .await
            .context("Mac keyboard layout query timed out")??;
            ensure!(kind == native::EVENT, "invalid keyboard layout response");
            crate::keymap::from_event(&serde_json::from_slice(&bytes)?)
        }
        .await;
        let _ = child.kill().await;
        result
    }
    #[cfg(not(target_os = "macos"))]
    Ok(String::new())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn cursor_in_video(headless: bool, features: &[String]) -> bool {
    // Older physical-output servers ignore the request and send cursor
    // shapes separately. Headless Hyprland frames already paint the cursor.
    headless || features.iter().any(|f| f == FEATURE_CURSOR_IN_VIDEO)
}

fn server_payload(m: &ServerMsg) -> u32 {
    match m {
        ServerMsg::VideoFrame {
            data_len, aux_len, ..
        } => data_len.saturating_add(*aux_len),
        ServerMsg::CursorShape { argb_len, .. } => *argb_len,
        ServerMsg::ClipboardData { data_len, .. } => *data_len,
        //= The Mac does not ask for sound, but a server that sends it anyway
        //= must not desynchronise the stream: read past the payload.
        ServerMsg::AudioFrame { data_len, .. } => *data_len,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn the_hub_routes_events_by_window() {
        let routes: Routes = Default::default();
        let (t0, mut r0) = mpsc::unbounded_channel();
        let (t1, mut r1) = mpsc::unbounded_channel();
        routes.borrow_mut().insert(0, t0);
        routes.borrow_mut().insert(1, t1);
        let (app_tx, mut app) = mpsc::unbounded_channel();
        let next = |rx: &mut mpsc::UnboundedReceiver<Result<Value>>| {
            rx.try_recv().ok().and_then(Result::ok)
        };
        route_event(&routes, &app_tx, json!({"type": "decoded", "window": 1}));
        assert_eq!(next(&mut r1).unwrap()["type"], "decoded");
        assert!(next(&mut r0).is_none(), "only the named window");
        route_event(&routes, &app_tx, json!({"type": "keyboard_layout"}));
        assert!(
            next(&mut r0).is_some() && next(&mut r1).is_some(),
            "every window"
        );
        route_event(&routes, &app_tx, json!({"type": "new_tab"}));
        assert_eq!(app.try_recv().unwrap()["type"], "new_tab");
        route_event(&routes, &app_tx, json!({"type": "closed", "window": 1}));
        assert_eq!(next(&mut r1).unwrap()["type"], "closed");
        assert!(
            !routes.borrow().contains_key(&1),
            "a closed tab's route is dropped"
        );
        route_event(&routes, &app_tx, json!({"type": "bye"}));
        assert!(next(&mut r0).is_some());
        assert_eq!(
            app.try_recv().unwrap()["type"],
            "bye",
            "quitting reaches the app too"
        );
        route_event(&routes, &app_tx, json!({"type": "decoded", "window": 9}));
        assert!(
            app.try_recv().is_err(),
            "an unknown window's event is dropped"
        );
    }

    #[test]
    fn a_first_connect_that_fails_waits_for_the_user() {
        //= A wrong password or refused key must not reopen the SSH prompt.
        let mut r = Retry::default();
        assert_eq!(r.failed(false, false), Next::Wait);
        assert_eq!(r.failed(false, false), Next::Wait);
    }

    #[test]
    fn a_dropped_session_retries_up_to_the_limit_then_waits() {
        let mut r = Retry::default();
        assert_eq!(r.failed(true, false), Next::Retry { attempt: 1 });
        for attempt in 2..=MAX_RETRIES {
            assert_eq!(r.failed(false, false), Next::Retry { attempt });
        }
        assert_eq!(r.failed(false, false), Next::Wait);
        assert_eq!(
            r.failed(false, false),
            Next::Wait,
            "no retries until one gets through"
        );
    }

    #[test]
    fn a_session_that_gets_through_resets_the_count() {
        let mut r = Retry::default();
        r.failed(true, false);
        r.failed(false, false);
        assert_eq!(r.failed(true, false), Next::Retry { attempt: 1 });
    }

    #[test]
    fn an_incompatible_server_is_never_retried() {
        let mut r = Retry::default();
        assert_eq!(r.failed(true, true), Next::Wait);
        let e: anyhow::Error = Incompatible("protocol 2".into()).into();
        assert!(e
            .context("ssh session")
            .downcast_ref::<Incompatible>()
            .is_some());
    }

    #[test]
    fn audio_payloads_are_skipped_not_parsed() {
        let frame = ServerMsg::AudioFrame {
            seq: 7,
            data_len: 960,
        };
        assert_eq!(server_payload(&frame), 960);
    }

    #[cfg(unix)]
    #[test]
    fn ssh_config_port_is_preserved_unless_selected_or_overridden() {
        use std::io::Write;
        for (args, expected) in [
            (vec!["gliff", "office"], "port 2222"),
            (vec!["gliff"], "port 4422"),
            (vec!["gliff", "office", "--port", "9922"], "port 9922"),
            (vec!["gliff", "--port", "9922"], "port 9922"),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let selected = crate::discovery::Selection {
                host: "office".into(),
                port: 4422,
                screen: crate::discovery::ScreenMode::Mirror,
            };
            let target = cli.ssh_target(&selected);
            let mut ssh = std::process::Command::new("ssh")
                .args(["-G", "-F", "/dev/stdin"])
                .args(&target.ssh_args)
                .arg(&target.host)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            ssh.stdin
                .take()
                .unwrap()
                .write_all(b"Host office\n  HostName 127.0.0.1\n  Port 2222\n")
                .unwrap();
            let output = ssh.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == expected));
        }
    }

    #[tokio::test]
    async fn pace_reconfiguration_keeps_decoder_reference_frames() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, server) = tokio::io::duplex(65536);
                let (client_rd, client_wr) = tokio::io::split(client);
                let (server_rd, server_wr) = tokio::io::split(server);
                let server = tokio::task::spawn_local(async move {
                    let mut rd = Framed::new(server_rd);
                    let mut wr = Framed::new(server_wr);
                    rd.read_msg::<ClientMsg>().await?;
                    wr.write_msg(&ServerMsg::HelloAck {
                        version: PROTOCOL_VERSION,
                        session: gliff_proto::SessionInfo {
                            headless: false,
                            output: "fixture".into(),
                        },
                        outputs: vec![],
                        features: vec![],
                    })
                    .await?;
                    let config = |fps_cap| ServerMsg::StreamConfig {
                        codec: Codec::H264,
                        chroma: ChromaMode::Single420,
                        pipeline: Some(gliff_proto::VideoPipeline::Cpu),
                        width: 16,
                        height: 16,
                        scale_milli: 1000,
                        extradata: vec![],
                        aux_extradata: None,
                        view_width: 16,
                        view_height: 16,
                        fps_cap,
                    };
                    wr.write_msg(&config(30)).await?;
                    let mut encoder = gliff_sw::Encoder::new(
                        gliff_sw::EncoderSettings {
                            width: 16,
                            height: 16,
                            bitrate: 100000,
                            framerate: 30,
                        },
                        false,
                    )?;
                    for frame_id in 1..=8 {
                        if frame_id == 4 {
                            wr.write_msg(&config(10)).await?;
                        }
                        let frame = encoder.encode_bgra(
                            &vec![(frame_id * 20) as u8; 16 * 16 * 4],
                            frame_id == 1,
                        )?;
                        wr.write_msg_with_payloads(
                            &ServerMsg::VideoFrame {
                                frame_id,
                                pts_us: frame_id * 33000,
                                keyframe: frame.keyframe,
                                damage: vec![],
                                data_len: frame.main.len() as u32,
                                aux_len: 0,
                            },
                            &[&frame.main],
                        )
                        .await?;
                    }
                    // Keep the pipe open while the decoder drains its pending picture.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Ok::<_, anyhow::Error>(())
                });
                let result = tokio::time::timeout(
                    Duration::from_secs(3),
                    session(client_rd, client_wr, "fixture", Some(4)),
                )
                .await?;
                server.abort();
                result
            })
            .await
    }
    #[tokio::test(start_paused = true)]
    async fn idle_connection_stays_alive_with_pong_replies() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, server) = tokio::io::duplex(65536);
                let (client_rd, client_wr) = tokio::io::split(client);
                let (server_rd, server_wr) = tokio::io::split(server);
                let peer = tokio::task::spawn_local(async move {
                    let mut rd = Framed::new(server_rd);
                    let mut wr = Framed::new(server_wr);
                    rd.read_msg::<ClientMsg>().await?;
                    wr.write_msg(&ServerMsg::HelloAck {
                        version: PROTOCOL_VERSION,
                        session: gliff_proto::SessionInfo {
                            headless: false,
                            output: "idle".into(),
                        },
                        outputs: vec![],
                        features: vec![],
                    })
                    .await?;
                    let started = tokio::time::Instant::now();
                    let mut pings = 0;
                    while let ClientMsg::Ping { t } = rd.read_msg::<ClientMsg>().await? {
                        wr.write_msg(&ServerMsg::Pong {
                            t,
                            server_now_ms: now_ms(),
                        })
                        .await?;
                        pings += 1;
                        if started.elapsed() >= Duration::from_secs(30) {
                            return Ok::<_, anyhow::Error>(pings);
                        }
                    }
                    anyhow::bail!("unexpected idle input")
                });
                let result = session(client_rd, client_wr, "idle", Some(1)).await;
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("connection closed"));
                assert!(peer.await?? >= 7);
                Ok(())
            })
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn silent_peer_still_times_out() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, _silent_peer) = tokio::io::duplex(65536);
                let (rd, wr) = tokio::io::split(client);
                let started = tokio::time::Instant::now();
                let result = session(rd, wr, "silent", Some(1)).await;
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("stopped responding"));
                assert_eq!(started.elapsed(), Duration::from_secs(20));
                Ok(())
            })
            .await
    }

    #[test]
    fn cursor_visibility_follows_the_server_capability() {
        assert!(!cursor_in_video(false, &[])); // older mirrored server
        assert!(cursor_in_video(true, &[])); // older headless server
        assert!(cursor_in_video(false, &[FEATURE_CURSOR_IN_VIDEO.into()]));
    }

    #[test]
    fn native_positions_become_evdev_and_pointer_messages() {
        assert_eq!(
            input(&json!({"type":"key", "keycode":0, "pressed":true})),
            Some(ClientMsg::Key {
                keycode: 30,
                pressed: true
            })
        );
        assert_eq!(
            input(&json!({"type":"button", "button":1, "pressed":false})),
            Some(ClientMsg::PointerButton {
                button: 273,
                pressed: false
            })
        );
        assert_eq!(
            input(&json!({"type":"motion", "x":100.5, "y":200.0})),
            Some(ClientMsg::PointerMotion { x: 100.5, y: 200.0 })
        );
        assert_eq!(
            input(&json!({"type":"key", "keycode":65536, "pressed":true})),
            None
        );
    }

    #[test]
    fn retina_resize_preserves_aspect_and_logical_coordinates() {
        for (width, height, scale) in [(2940, 1912, 2.0), (4112, 2658, 2.0), (6016, 3384, 2.0)] {
            let Some(ClientMsg::Resize {
                width: w,
                height: h,
                scale: s,
            }) = input(&json!({"type":"resize", "width":width, "height":height, "scale":scale}))
            else {
                panic!("missing resize")
            };
            assert!(w <= 3840 && h <= 2160);
            assert!(
                (f64::from(w) / f64::from(h) - f64::from(width) / f64::from(height)).abs() < 0.001
            );
            assert!((f64::from(w) / f64::from(s) - f64::from(width) / scale).abs() < 1.0);
            assert!((f64::from(h) / f64::from(s) - f64::from(height) / scale).abs() < 1.0);
        }
        assert!(input(&json!({"type":"resize", "width":0, "height":720, "scale":2})).is_none());
        assert!(input(&json!({"type":"resize", "width":1100, "height":720, "scale":0})).is_none());
    }
}
