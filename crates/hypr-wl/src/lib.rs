//! Shared Wayland plumbing for gliff's compositor-facing crates: connecting to
//! a chosen socket, listing globals, and tracking `wl_output`s and the seat.

#![forbid(unsafe_code)]

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use calloop::channel::{self, Channel};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("XDG_RUNTIME_DIR is not set and /run/user/<uid> does not exist")]
    NoRuntimeDir,
    #[error("no Wayland display: set WAYLAND_DISPLAY or run inside Hyprland ({0})")]
    NoDisplay(String),
    #[error("connect to {0}: {1}")]
    Connect(PathBuf, std::io::Error),
    #[error("wayland: {0}")]
    Wayland(String),
    #[error("compositor does not offer {0}")]
    MissingGlobal(&'static str),
    #[error("no output named {0}")]
    NoOutput(String),
}

impl From<wayland_client::globals::GlobalError> for Error {
    fn from(e: wayland_client::globals::GlobalError) -> Self {
        Error::Wayland(e.to_string())
    }
}

impl From<wayland_client::globals::BindError> for Error {
    fn from(e: wayland_client::globals::BindError) -> Self {
        Error::Wayland(e.to_string())
    }
}

impl From<wayland_client::DispatchError> for Error {
    fn from(e: wayland_client::DispatchError) -> Self {
        Error::Wayland(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Which compositor socket to talk to.
#[derive(Debug, Clone, Default)]
pub struct Target {
    /// Wayland socket name (`wayland-1`); defaults to `WAYLAND_DISPLAY`, then
    /// the discovered Hyprland instance's socket.
    pub display: Option<String>,
    /// Hyprland instance signature override.
    pub instance: Option<String>,
}

impl Target {
    pub fn resolve_display(&self) -> Result<String> {
        if let Some(d) = &self.display {
            return Ok(d.clone());
        }
        if self.instance.is_none() {
            if let Ok(d) = std::env::var("WAYLAND_DISPLAY") {
                if !d.is_empty() {
                    return Ok(d);
                }
            }
        }
        let inst = hypr_ipc::Instance::discover(self.instance.as_deref())
            .map_err(|e| Error::NoDisplay(e.to_string()))?;
        inst.wayland_display()
            .map_err(|e| Error::NoDisplay(e.to_string()))
    }

    pub fn instance(&self) -> Result<hypr_ipc::Instance> {
        hypr_ipc::Instance::discover(self.instance.as_deref())
            .map_err(|e| Error::NoDisplay(e.to_string()))
    }
}

pub fn connect(target: &Target) -> Result<Connection> {
    let display = target.resolve_display()?;
    let path = if display.starts_with('/') {
        PathBuf::from(display)
    } else {
        let dir = hypr_ipc::runtime_dir().map_err(|_| Error::NoRuntimeDir)?;
        dir.join(display)
    };
    let stream = UnixStream::connect(&path).map_err(|e| Error::Connect(path.clone(), e))?;
    tracing::debug!(path = %path.display(), "connected to compositor");
    Connection::from_socket(stream).map_err(|e| Error::Wayland(e.to_string()))
}

/// Connect and fetch the global list for a state type `D`.
pub fn init<D>(target: &Target) -> Result<(Connection, GlobalList, EventQueue<D>)>
where
    D: Dispatch<WlRegistry, GlobalListContents> + 'static,
{
    let conn = connect(target)?;
    let (globals, queue) = registry_queue_init::<D>(&conn)?;
    Ok((conn, globals, queue))
}

#[derive(Debug, Clone)]
pub struct GlobalInfo {
    pub interface: String,
    pub version: u32,
}

pub fn list_globals(globals: &GlobalList) -> Vec<GlobalInfo> {
    let mut v: Vec<GlobalInfo> = globals.contents().with_list(|list| {
        list.iter()
            .map(|g| GlobalInfo {
                interface: g.interface.clone(),
                version: g.version,
            })
            .collect()
    });
    v.sort_by(|a, b| a.interface.cmp(&b.interface));
    v
}

pub fn has_global(globals: &GlobalList, interface: &str) -> bool {
    globals
        .contents()
        .with_list(|list| list.iter().any(|g| g.interface == interface))
}

/// State of one `wl_output`.
#[derive(Debug, Clone, Default)]
pub struct OutputInfo {
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    pub transform: u32,
    pub refresh_mhz: i32,
    pub done: bool,
}

impl OutputInfo {
    /// Logical size after scale and transform.
    pub fn logical_size(&self) -> (u32, u32) {
        let (w, h) = if self.transform % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let s = self.scale.max(1);
        ((w / s).max(1) as u32, (h / s).max(1) as u32)
    }
}

/// Tracks every `wl_output` advertised by the compositor.
#[derive(Debug, Default)]
pub struct Outputs {
    pub list: Vec<(WlOutput, OutputInfo)>,
}

impl Outputs {
    /// Bind every output global. Call `roundtrip` afterwards so names arrive.
    pub fn bind<D>(globals: &GlobalList, qh: &QueueHandle<D>) -> Result<Self>
    where
        D: Dispatch<WlOutput, ()> + 'static,
    {
        let mut list = Vec::new();
        let names: Vec<u32> = globals.contents().with_list(|l| {
            l.iter()
                .filter(|g| g.interface == WlOutput::interface().name)
                .map(|g| g.name)
                .collect()
        });
        for name in names {
            let output: WlOutput = globals.registry().bind(name, 4, qh, ());
            list.push((output, OutputInfo::default()));
        }
        Ok(Self { list })
    }

    pub fn get(&self, name: &str) -> Option<&(WlOutput, OutputInfo)> {
        self.list.iter().find(|(_, i)| i.name == name)
    }

    pub fn find(&self, name: &str) -> Result<(WlOutput, OutputInfo)> {
        self.get(name)
            .cloned()
            .ok_or_else(|| Error::NoOutput(name.to_owned()))
    }

    pub fn infos(&self) -> Vec<OutputInfo> {
        self.list.iter().map(|(_, i)| i.clone()).collect()
    }

    fn info_mut(&mut self, output: &WlOutput) -> Option<&mut OutputInfo> {
        self.list
            .iter_mut()
            .find(|(o, _)| o == output)
            .map(|(_, i)| i)
    }

    /// Handle a `wl_output` event; returns true when an output finished a burst.
    pub fn handle(&mut self, output: &WlOutput, event: wl_output::Event) -> bool {
        let Some(info) = self.info_mut(output) else {
            return false;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                info.x = x;
                info.y = y;
                info.transform = transform.into();
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                if flags
                    .into_result()
                    .map(|f| f.contains(wl_output::Mode::Current))
                    .unwrap_or(false)
                {
                    info.width = width;
                    info.height = height;
                    info.refresh_mhz = refresh;
                }
            }
            wl_output::Event::Scale { factor } => info.scale = factor,
            wl_output::Event::Name { name } => info.name = name,
            wl_output::Event::Description { description } => info.description = description,
            wl_output::Event::Done => {
                info.done = true;
                return true;
            }
            _ => {}
        }
        false
    }
}

/// Tracks the seat and its capabilities.
#[derive(Debug, Default)]
pub struct Seat {
    pub seat: Option<WlSeat>,
    pub name: String,
    pub has_pointer: bool,
    pub has_keyboard: bool,
}

impl Seat {
    pub fn bind<D>(globals: &GlobalList, qh: &QueueHandle<D>) -> Result<Self>
    where
        D: Dispatch<WlSeat, ()> + 'static,
    {
        let seat: WlSeat = globals.bind(qh, 1..=7, ())?;
        Ok(Self {
            seat: Some(seat),
            ..Default::default()
        })
    }

    pub fn handle(&mut self, event: wl_seat::Event) {
        match event {
            wl_seat::Event::Capabilities { capabilities } => {
                if let Ok(caps) = capabilities.into_result() {
                    self.has_pointer = caps.contains(wl_seat::Capability::Pointer);
                    self.has_keyboard = caps.contains(wl_seat::Capability::Keyboard);
                }
            }
            wl_seat::Event::Name { name } => self.name = name,
            _ => {}
        }
    }

    pub fn wl_seat(&self) -> Result<&WlSeat> {
        self.seat.as_ref().ok_or(Error::MissingGlobal("wl_seat"))
    }
}

/// A thread state driven by [`run_loop`]: Wayland events dispatch into it and
/// commands from the owning thread arrive through [`LoopState::on_cmd`].
pub trait LoopState: 'static {
    type Cmd;
    fn on_cmd(&mut self, cmd: Self::Cmd);
    /// Ask the loop to end; also called when the command channel closes
    /// (the owner dropped its handle).
    fn stop(&mut self);
    fn stop_requested(&self) -> bool;
}

/// Run a calloop loop over the Wayland queue and the command channel until
/// the state reports it has stopped.
pub fn run_loop<S: LoopState>(
    conn: Connection,
    queue: EventQueue<S>,
    rx: Channel<S::Cmd>,
    state: &mut S,
) -> Result<()> {
    let wayland = |e: &dyn std::fmt::Display| Error::Wayland(e.to_string());
    let mut event_loop: EventLoop<S> = EventLoop::try_new().map_err(|e| wayland(&e))?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, queue)
        .insert(handle.clone())
        .map_err(|e| wayland(&e))?;
    handle
        .insert_source(rx, |evt, _, state: &mut S| match evt {
            channel::Event::Msg(cmd) => state.on_cmd(cmd),
            channel::Event::Closed => state.stop(),
        })
        .map_err(|e| wayland(&e))?;
    let signal = event_loop.get_signal();
    event_loop
        .run(Duration::from_millis(500), state, |state| {
            if state.stop_requested() {
                signal.stop();
            }
        })
        .map_err(|e| wayland(&e))
}

/// Milliseconds since an arbitrary monotonic origin, for input timestamps.
pub fn now_ms() -> u32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u32
}
