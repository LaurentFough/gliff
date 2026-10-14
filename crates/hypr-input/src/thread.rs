use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use calloop::channel::{self, Sender};
use nix::sys::memfd::{memfd_create, MFdFlags};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use crate::keymap::KeyState;
use crate::{Axis, Error, EventSink, InputCmd, InputConfig, InputEvent, Result};
use hypr_wl::{now_ms, LoopState, Outputs, Seat};

const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// How long a keyboard replaced by a keymap change lives on. Destroying it at
/// once, while an input method (fcitx5) may still be handling its last keys,
/// left a stray modifier on those keys.
const RETIRED_KEYBOARD_GRACE: Duration = Duration::from_secs(1);

struct State {
    sink: EventSink,
    conn: Connection,
    outputs: Outputs,
    seat: Seat,
    qh: QueueHandle<State>,
    kb_mgr: ZwpVirtualKeyboardManagerV1,
    seat_proxy: WlSeat,
    keyboard: ZwpVirtualKeyboardV1,
    /// Keyboards replaced by a keymap change, with when, until their grace
    /// period is over.
    retired: Vec<(Instant, ZwpVirtualKeyboardV1)>,
    pointer: ZwlrVirtualPointerV1,
    keys: KeyState,
    pressed_keys: BTreeSet<u32>,
    pressed_buttons: BTreeSet<u32>,
    extent: (u32, u32),
    quit: bool,
}

pub fn spawn(
    cfg: InputConfig,
    sink: EventSink,
    ready_tx: mpsc::Sender<Result<()>>,
) -> Result<(Sender<InputCmd>, std::thread::JoinHandle<()>)> {
    let (tx, rx) = channel::channel::<InputCmd>();
    let join = std::thread::Builder::new()
        .name("hypr-input".into())
        .spawn(move || {
            let mut sink = sink;
            // `run` reports the error through the sink itself; this send only
            // matters when it failed before signalling ready.
            if let Err(e) = run(cfg, &mut sink, rx, ready_tx.clone()) {
                let _ = ready_tx.send(Err(Error::Input(e.to_string())));
            }
        })?;
    Ok((tx, join))
}

fn run(
    cfg: InputConfig,
    sink: &mut EventSink,
    rx: channel::Channel<InputCmd>,
    ready_tx: mpsc::Sender<Result<()>>,
) -> Result<()> {
    let (conn, globals, queue) = hypr_wl::init::<State>(&cfg.target)?;
    let qh = queue.handle();
    let kb_mgr: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| hypr_wl::Error::MissingGlobal("zwp_virtual_keyboard_manager_v1"))?;
    let ptr_mgr: ZwlrVirtualPointerManagerV1 = globals
        .bind(&qh, 2..=2, ())
        .map_err(|_| hypr_wl::Error::MissingGlobal("zwlr_virtual_pointer_manager_v1 v2"))?;
    let outputs = Outputs::bind(&globals, &qh)?;
    let seat = Seat::bind(&globals, &qh)?;

    let keys = match &cfg.keymap {
        Some(text) => KeyState::from_text(text)?,
        None => KeyState::default_us()?,
    };
    let seat_proxy = seat.wl_seat()?.clone();
    let keyboard = kb_mgr.create_virtual_keyboard(&seat_proxy, &qh, ());
    let mut state = State {
        sink: Box::new(|_| {}),
        conn: conn.clone(),
        outputs,
        seat,
        qh: qh.clone(),
        kb_mgr,
        seat_proxy: seat_proxy.clone(),
        keyboard,
        retired: Vec::new(),
        pointer: ptr_mgr.create_virtual_pointer(Some(&seat_proxy), &qh, ()),
        keys,
        pressed_keys: BTreeSet::new(),
        pressed_buttons: BTreeSet::new(),
        extent: (1, 1),
        quit: false,
    };
    std::mem::swap(&mut state.sink, sink);
    let result = input_loop(
        &mut state,
        conn,
        queue,
        rx,
        ready_tx,
        &cfg,
        &qh,
        &ptr_mgr,
        &seat_proxy,
    );
    if let Err(e) = &result {
        state.emit(InputEvent::Error(e.to_string()));
    }
    state.release_all();
    state.keyboard.destroy();
    for (_, keyboard) in state.retired.drain(..) {
        keyboard.destroy();
    }
    state.pointer.destroy();
    let _ = state.conn.flush();
    std::mem::swap(&mut state.sink, sink);
    result
}

#[allow(clippy::too_many_arguments)]
fn input_loop(
    state: &mut State,
    conn: Connection,
    mut queue: wayland_client::EventQueue<State>,
    rx: channel::Channel<InputCmd>,
    ready_tx: mpsc::Sender<Result<()>>,
    cfg: &InputConfig,
    qh: &QueueHandle<State>,
    ptr_mgr: &ZwlrVirtualPointerManagerV1,
    seat_proxy: &WlSeat,
) -> Result<()> {
    queue.roundtrip(state)?;
    queue.roundtrip(state)?;

    let (output, info) = state.outputs.find(&cfg.output)?;
    state.extent = info.logical_size();
    let bound = ptr_mgr.create_virtual_pointer_with_output(Some(seat_proxy), Some(&output), qh, ());
    let old = std::mem::replace(&mut state.pointer, bound);
    old.destroy();
    state.upload_keymap()?;
    tracing::info!(output = %info.name, extent = ?state.extent, "input ready");
    state.emit(InputEvent::Ready);
    let _ = ready_tx.send(Ok(()));
    Ok(hypr_wl::run_loop(conn, queue, rx, state)?)
}

impl State {
    fn emit(&mut self, ev: InputEvent) {
        (self.sink)(ev);
    }

    fn upload_keymap(&mut self) -> Result<()> {
        Self::upload_keymap_to(&self.keyboard, &self.keys.text)
    }

    fn upload_keymap_to(keyboard: &ZwpVirtualKeyboardV1, text: &str) -> Result<()> {
        let fd = memfd_create(c"gliff-keymap", MFdFlags::MFD_CLOEXEC)
            .map_err(|e| Error::Input(format!("memfd: {e}")))?;
        let mut file = File::from(fd);
        file.write_all(text.as_bytes())?;
        file.write_all(&[0])?;
        file.flush()?;
        let size = text.len() as u32 + 1;
        keyboard.keymap(KEYMAP_FORMAT_XKB_V1, file.as_fd(), size);
        Ok(())
    }

    fn destroy_retired_keyboards(&mut self) {
        let now = Instant::now();
        self.retired.retain(|(since, keyboard)| {
            let keep = now.duration_since(*since) < RETIRED_KEYBOARD_GRACE;
            if !keep {
                keyboard.destroy();
            }
            keep
        });
    }

    fn release_all(&mut self) {
        let t = now_ms();
        for code in std::mem::take(&mut self.pressed_keys) {
            self.keyboard.key(t, code, 0);
        }
        self.keys.reset();
        self.keyboard.modifiers(0, 0, 0, 0);
        for b in std::mem::take(&mut self.pressed_buttons) {
            self.pointer.button(t, b, wl_pointer::ButtonState::Released);
        }
        self.pointer.frame();
    }
}

impl LoopState for State {
    type Cmd = InputCmd;

    fn on_cmd(&mut self, cmd: InputCmd) {
        self.destroy_retired_keyboards();
        let t = now_ms();
        match cmd {
            InputCmd::Key { code, pressed } => {
                if pressed {
                    self.pressed_keys.insert(code);
                } else {
                    self.pressed_keys.remove(&code);
                }
                self.keyboard.key(t, code, if pressed { 1 } else { 0 });
                if let Some((dep, lat, lock, group)) = self.keys.update(code, pressed) {
                    self.keyboard.modifiers(dep, lat, lock, group);
                }
            }
            InputCmd::Motion { x, y } => {
                let (w, h) = self.extent;
                let xi = x.clamp(0.0, (w.saturating_sub(1)) as f64).round() as u32;
                let yi = y.clamp(0.0, (h.saturating_sub(1)) as f64).round() as u32;
                self.pointer.motion_absolute(t, xi, yi, w, h);
                self.pointer.frame();
            }
            InputCmd::Button { button, pressed } => {
                if pressed {
                    self.pressed_buttons.insert(button);
                } else {
                    self.pressed_buttons.remove(&button);
                }
                let st = if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                self.pointer.button(t, button, st);
                self.pointer.frame();
            }
            InputCmd::Axis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let a = match axis {
                    Axis::Vertical => wl_pointer::Axis::VerticalScroll,
                    Axis::Horizontal => wl_pointer::Axis::HorizontalScroll,
                };
                let source = if discrete.is_some() {
                    wl_pointer::AxisSource::Wheel
                } else {
                    wl_pointer::AxisSource::Finger
                };
                self.pointer.axis_source(source);
                if stop {
                    self.pointer.axis_stop(t, a);
                } else if let Some(d) = discrete {
                    self.pointer.axis_discrete(t, a, value, d);
                } else {
                    self.pointer.axis(t, a, value);
                }
                self.pointer.frame();
            }
            InputCmd::SetKeymap(text) if text == self.keys.text => {}
            InputCmd::SetKeymap(text) => match KeyState::from_text(&text) {
                Ok(mut ks) => {
                    let (dep, lat, lock, group) =
                        ks.carry_over(&self.keys, self.pressed_keys.iter().copied());
                    // A new keyboard rather than a new keymap on this one: with
                    // an input method active (fcitx5 on Omarchy) Hyprland routes
                    // keys through its keyboard grab, which keeps the first
                    // keymap it saw from a keyboard and so ignores a re-upload.
                    let keyboard =
                        self.kb_mgr
                            .create_virtual_keyboard(&self.seat_proxy, &self.qh, ());
                    if let Err(e) = Self::upload_keymap_to(&keyboard, &ks.text) {
                        keyboard.destroy();
                        self.emit(InputEvent::Error(e.to_string()));
                        return;
                    }
                    // Held keys belong to a Wayland keyboard, not just its
                    // XKB state. Release the retiring device before replaying
                    // them on its replacement, so later key-ups reach their
                    // owner and retirement cannot release a newly held key.
                    for &code in &self.pressed_keys {
                        self.keyboard.key(t, code, 0);
                    }
                    self.keyboard.modifiers(0, 0, 0, 0);
                    let old = std::mem::replace(&mut self.keyboard, keyboard);
                    self.retired.push((Instant::now(), old));
                    self.keys = ks;
                    for &code in &self.pressed_keys {
                        self.keyboard.key(t, code, 1);
                    }
                    self.keyboard.modifiers(dep, lat, lock, group);
                }
                Err(e) => self.emit(InputEvent::Error(e.to_string())),
            },
            InputCmd::SetExtent { width, height } => self.extent = (width.max(1), height.max(1)),
            InputCmd::ReleaseAll => self.release_all(),
            InputCmd::Stop => self.stop(),
        }
        let _ = self.conn.flush();
    }

    fn stop(&mut self) {
        self.quit = true;
    }

    fn stop_requested(&self) -> bool {
        self.quit
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
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

impl Dispatch<WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        o: &WlOutput,
        e: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.outputs.handle(o, e);
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        s: &mut Self,
        _: &WlSeat,
        e: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.seat.handle(e);
    }
}

delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ZwlrVirtualPointerV1);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use wayland_client::Proxy;

    struct Registry;
    impl Dispatch<WlRegistry, ()> for Registry {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wayland_client::protocol::wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    // Decode the actual outgoing Wayland key requests, including their device
    // IDs. No compositor is needed: the socket peer records requests only.
    fn key_requests(peer: &mut UnixStream) -> Vec<(u32, u32, bool)> {
        let mut bytes = Vec::new();
        let mut buf = [0; 4096];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("read Wayland requests: {e}"),
            }
        }
        let mut keys = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let word = |offset| u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let id = word(at);
            let header = word(at + 4);
            let size = (header >> 16) as usize;
            assert!(size >= 8 && at + size <= bytes.len());
            if header & 0xffff == 1 && size == 20 {
                keys.push((id, word(at + 12), word(at + 16) != 0));
            }
            at += size;
        }
        keys
    }

    #[test]
    fn held_keys_move_to_the_replacement_keyboard() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let conn = Connection::from_socket(socket).unwrap();
        let registry_queue = conn.new_event_queue::<Registry>();
        let registry = conn.display().get_registry(&registry_queue.handle(), ());
        let queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        let kb_mgr: ZwpVirtualKeyboardManagerV1 = registry.bind(1, 1, &qh, ());
        let seat_proxy: WlSeat = registry.bind(2, 1, &qh, ());
        let ptr_mgr: ZwlrVirtualPointerManagerV1 = registry.bind(3, 2, &qh, ());
        let keyboard = kb_mgr.create_virtual_keyboard(&seat_proxy, &qh, ());
        let old_id = keyboard.id().protocol_id();
        let mut state = State {
            sink: Box::new(|_| {}),
            conn: conn.clone(),
            outputs: Outputs::default(),
            seat: Seat::default(),
            qh: qh.clone(),
            kb_mgr,
            seat_proxy: seat_proxy.clone(),
            keyboard,
            retired: Vec::new(),
            pointer: ptr_mgr.create_virtual_pointer(Some(&seat_proxy), &qh, ()),
            keys: KeyState::default_us().unwrap(),
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            extent: (1, 1),
            quit: false,
        };
        state.upload_keymap().unwrap();
        state.on_cmd(InputCmd::Key {
            code: 42,
            pressed: true,
        });
        state.on_cmd(InputCmd::Key {
            code: 16,
            pressed: true,
        });
        key_requests(&mut peer);
        let text = crate::keymap_from_names(&crate::KeymapNames {
            layout: "fr".into(),
            ..Default::default()
        })
        .unwrap();
        state.on_cmd(InputCmd::SetKeymap(text));
        let new_id = state.keyboard.id().protocol_id();
        assert_ne!(old_id, new_id);
        assert_eq!(
            key_requests(&mut peer),
            vec![
                (old_id, 16, false),
                (old_id, 42, false),
                (new_id, 16, true),
                (new_id, 42, true),
            ]
        );
        state.on_cmd(InputCmd::Key {
            code: 16,
            pressed: false,
        });
        state.on_cmd(InputCmd::Key {
            code: 42,
            pressed: false,
        });
        assert_eq!(
            key_requests(&mut peer),
            vec![(new_id, 16, false), (new_id, 42, false)]
        );
        state.retired[0].0 -= RETIRED_KEYBOARD_GRACE;
        state.destroy_retired_keyboards();
        conn.flush().unwrap();
        assert!(key_requests(&mut peer).is_empty());
        assert!(state.retired.is_empty());
    }
}
