//! Main-thread UI state and the logic between AppKit events and the worker.
//!
//! The state lives in a thread-local so the view, the key monitor and the
//! timer can all reach it without retaining each other. Callers never hold
//! the borrow across a call that could re-enter AppKit event delivery.

use std::cell::RefCell;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use gliff_proto::{Axis, ClientMsg};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use tokio::sync::mpsc::UnboundedSender;

use super::cg;
use super::view::GliffView;
use crate::geometry::{self, Layout, StreamGeometry};
use crate::hotkey::HotkeyMatcher;
use crate::keys::{self, ev, flag, vk, HeldKeys, KeyOptions};
use crate::session::{Picture, Status, ToWorker};

//= Version of the UI behaviour.
pub const UI_VERSION: &str = "0.1.0";

/// The window asks for a new remote size once its size has held this long.
const RESIZE_SETTLE: Duration = Duration::from_millis(200);
/// How long a hint stays in the subtitle.
const HINT_FOR: Duration = Duration::from_secs(4);
/// `NSEventPhaseEnded | NSEventPhaseCancelled`.
const PHASE_END: usize = 0x08 | 0x10;
/// `NSEventType` values of the key events (their mask bit is `1 << type`).
const EVENT_KEY_DOWN: u32 = 10;
const EVENT_KEY_UP: u32 = 11;
const EVENT_FLAGS_CHANGED: u32 = 12;

pub struct UiState {
    pub input: Option<UnboundedSender<ToWorker>>,
    pub frames: Receiver<Picture>,
    pub status: Receiver<Status>,
    pub keys: KeyOptions,
    pub hotkey: HotkeyMatcher,
    pub held: HeldKeys,
    pub host: String,
    pub window: Retained<AnyObject>,
    pub view: Retained<GliffView>,
    /// The sublayer showing the picture.
    pub image_layer: Retained<AnyObject>,
    /// Geometry of the picture on screen (pointer mapping uses it).
    pub geometry: Option<StreamGeometry>,
    pub headless: bool,
    /// The latest StreamConfig's view, for the resize gate.
    pub server_view: (u32, u32),
    pub inside: bool,
    /// The release hotkey was pressed; recapture on click or re-entry.
    pub released: bool,
    pub was_captured: bool,
    pub cursor: Option<Retained<AnyObject>>,
    pub hidden_cursor: Option<Retained<AnyObject>>,
    pub last_layout: Option<(Layout, (u32, u32))>,
    /// Size seen on the last tick and since when.
    pub resize_seen: Option<((u32, u32), f64, Instant)>,
    pub resize_sent: Option<((u32, u32), f64)>,
    pub line: String,
    pub hint: Option<(String, Instant)>,
    pub shown_subtitle: String,
}

thread_local! {
    static STATE: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

pub fn install(state: UiState) {
    STATE.with(|s| *s.borrow_mut() = Some(state));
}

/// Take the state out (on quit).
pub fn take() -> Option<UiState> {
    STATE.with(|s| s.borrow_mut().take())
}

/// Run `f` on the state when it is installed and not already borrowed.
fn with<R>(f: impl FnOnce(&mut UiState) -> R) -> Option<R> {
    STATE.with(|s| match s.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => {
            tracing::debug!("ui state busy; event dropped");
            None
        }
    })
}

impl UiState {
    fn send(&self, msg: ClientMsg) {
        if let Some(tx) = &self.input {
            let _ = tx.send(ToWorker::Send(msg));
        }
    }

    fn captured(&self) -> bool {
        self.inside && !self.released && self.window_is_key() && self.geometry.is_some()
    }

    fn window_is_key(&self) -> bool {
        // SAFETY: NSWindow getter on the main thread.
        unsafe { msg_send![&*self.window, isKeyWindow] }
    }

    fn release_all_keys(&mut self) {
        for code in self.held.drain() {
            self.send(ClientMsg::Key {
                keycode: code,
                pressed: false,
            });
        }
    }

    fn set_hint(&mut self, text: impl Into<String>) {
        self.hint = Some((text.into(), Instant::now()));
    }
}

//= ---------------------------------------------------------------------
//= Pointer
//= ---------------------------------------------------------------------

pub fn pointer_moved(point: (f64, f64)) {
    with(|s| {
        let Some(geom) = s.geometry else { return };
        let device = s.view.device_scale();
        if let Some((x, y)) = geometry::to_remote(point, geom, device, s.view.size()) {
            s.send(ClientMsg::PointerMotion { x, y });
        }
    });
}

pub fn button(number: i64, pressed: bool) {
    with(|s| {
        if pressed {
            s.released = false;
            s.inside = true;
        }
        s.send(ClientMsg::PointerButton {
            button: keys::evdev_button(number),
            pressed,
        });
    });
}

pub fn scrolled(dx: f64, dy: f64, precise: bool, phase: usize, momentum: usize) {
    with(|s| {
        //= The remote app scrolls kinetically itself after `stop`; skip
        //= macOS's own momentum events so it does not happen twice.
        if momentum != 0 {
            return;
        }
        for e in keys::scroll(dx, dy, precise) {
            s.send(ClientMsg::PointerAxis {
                axis: e.axis,
                value: e.value,
                discrete: e.discrete,
                stop: false,
            });
        }
        if precise && phase & PHASE_END != 0 {
            for axis in [Axis::Vertical, Axis::Horizontal] {
                s.send(ClientMsg::PointerAxis {
                    axis,
                    value: 0.0,
                    discrete: None,
                    stop: true,
                });
            }
        }
    });
}

pub fn pointer_entered() {
    with(|s| {
        s.inside = true;
        s.released = false;
    });
    apply_cursor();
}

pub fn pointer_left() {
    with(|s| {
        s.inside = false;
        //= A deliberate release lasts while the pointer rests on the picture.
        s.released = false;
        s.release_all_keys();
    });
    // SAFETY: class method on the main thread.
    unsafe {
        let arrow: Retained<AnyObject> = msg_send![class!(NSCursor), arrowCursor];
        let _: () = msg_send![&*arrow, set];
    }
}

/// Show the remote's cursor shape (or none on a headless screen, whose
/// frames already contain the pointer) while over the picture.
pub fn apply_cursor() {
    let cursor = with(|s| {
        if !s.inside {
            return None;
        }
        s.cursor
            .clone()
            .or_else(|| s.headless.then(|| s.hidden_cursor.clone()).flatten())
    })
    .flatten();
    if let Some(c) = cursor {
        // SAFETY: -[NSCursor set] on the main thread.
        unsafe {
            let _: () = msg_send![&*c, set];
        }
    }
}

//= ---------------------------------------------------------------------
//= Keyboard (called from the local NSEvent monitor)
//= ---------------------------------------------------------------------

/// The kinds of key event the monitors deliver (one monitor per kind, so
/// no `-[NSEvent type]` lookup is needed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Down,
    Up,
    FlagsChanged,
}

impl KeyKind {
    /// The `NSEventMask` bit for this kind.
    pub fn mask(self) -> u64 {
        1u64 << match self {
            Self::Down => EVENT_KEY_DOWN,
            Self::Up => EVENT_KEY_UP,
            Self::FlagsChanged => EVENT_FLAGS_CHANGED,
        }
    }
}

/// Handle a key event; true when it was consumed for the remote.
pub fn key_event(kind: KeyKind, event: &AnyObject) -> bool {
    // SAFETY: NSEvent getters valid for key and flags-changed events.
    let (code, flags): (u16, usize) =
        unsafe { (msg_send![event, keyCode], msg_send![event, modifierFlags]) };
    let flags = flags as u64;
    with(|s| {
        if !s.captured() {
            return false;
        }
        match kind {
            KeyKind::Down => {
                // SAFETY: valid for key-down events.
                let repeat: bool = unsafe { msg_send![event, isARepeat] };
                if repeat {
                    //= The remote compositor repeats on its own.
                    return true;
                }
                if s.hotkey.on_key_down(code, flags, Instant::now()) {
                    s.release_all_keys();
                    s.released = true;
                    let how = s.hotkey.hotkey().describe();
                    s.set_hint(format!(
                        "Keyboard released — click the screen to capture ({how})"
                    ));
                    return true;
                }
                if let Some(code) = keys::evdev_key(code, s.keys) {
                    if s.held.press(code) {
                        s.send(ClientMsg::Key {
                            keycode: code,
                            pressed: true,
                        });
                    }
                }
                true
            }
            KeyKind::Up => {
                if let Some(code) = keys::evdev_key(code, s.keys) {
                    if s.held.release(code) {
                        s.send(ClientMsg::Key {
                            keycode: code,
                            pressed: false,
                        });
                    }
                }
                true
            }
            KeyKind::FlagsChanged => {
                modifier_changed(s, code, flags);
                true
            }
        }
    })
    .unwrap_or(false)
}

fn modifier_changed(s: &mut UiState, code: u16, flags: u64) {
    if code == vk::CAPS_LOCK {
        //= macOS reports the lock state, not the key: send one tap, which
        //= toggles the remote's lock the same way.
        for pressed in [true, false] {
            s.send(ClientMsg::Key {
                keycode: ev::KEY_CAPSLOCK,
                pressed,
            });
        }
        return;
    }
    let (Some(down), Some(ev_code)) = (
        keys::modifier_down(code, flags),
        keys::evdev_key(code, s.keys),
    ) else {
        return;
    };
    if down {
        if s.held.press(ev_code) {
            s.send(ClientMsg::Key {
                keycode: ev_code,
                pressed: true,
            });
        }
    } else {
        //= macOS never sends key-up for keys released while Command was
        //= down; release them together with Command.
        if (code == vk::COMMAND || code == vk::RIGHT_COMMAND) && flags & flag::COMMAND == 0 {
            for k in s.held.drain_non_modifiers() {
                s.send(ClientMsg::Key {
                    keycode: k,
                    pressed: false,
                });
            }
        }
        if s.held.release(ev_code) {
            s.send(ClientMsg::Key {
                keycode: ev_code,
                pressed: false,
            });
        }
    }
}

//= ---------------------------------------------------------------------
//= Timer tick: worker → screen
//= ---------------------------------------------------------------------

/// Called ~120 times a second on the main thread.
pub fn tick() {
    let mut cursor_changed = false;
    with(|s| {
        drain_status(s, &mut cursor_changed);
        show_latest_frame(s);
        relayout(s);
        maybe_resize(s);
        track_capture(s);
        update_subtitle(s);
    });
    if cursor_changed {
        apply_cursor();
    }
}

fn drain_status(s: &mut UiState, cursor_changed: &mut bool) {
    while let Ok(status) = s.status.try_recv() {
        match status {
            Status::Connecting { attempt } => {
                //= A new session needs its own Resize.
                s.resize_sent = None;
                s.line = if attempt > 1 {
                    format!("Reconnecting (attempt {attempt})…")
                } else {
                    "Connecting…".into()
                };
            }
            Status::Connected {
                video,
                chroma,
                view,
                fps_cap,
                headless,
            } => {
                s.headless = headless;
                s.server_view = view;
                s.line = format!(
                    "{video} · {chroma:?} · {}x{} · cap {fps_cap} fps",
                    view.0, view.1
                );
                tracing::info!(%video, ?chroma, ?view, fps_cap, headless, "connected");
                *cursor_changed = true;
            }
            Status::Stats {
                fps,
                mbit,
                decode_ms,
            } => {
                s.line = format!("{fps:.0} fps · {mbit:.1} Mbit/s · decode {decode_ms:.1} ms");
            }
            Status::Cursor(slot) => {
                if let Some(shape) = slot.take() {
                    s.cursor = make_cursor(shape, s.view.device_scale());
                    *cursor_changed = true;
                }
            }
            Status::Error(e) => {
                s.line = format!("Disconnected: {e}");
                s.geometry = None;
                s.release_all_keys();
            }
            Status::Incompatible(e) => {
                s.line = format!("Incompatible server: {e}");
                s.geometry = None;
            }
            Status::Stopped => {
                tracing::info!(host = %s.host, "session worker stopped");
                if !s.line.starts_with("Disconnected") && !s.line.starts_with("Incompatible") {
                    s.line = "Stopped".into();
                }
            }
        }
    }
}

fn show_latest_frame(s: &mut UiState) {
    //= Latest wins: older pictures are dropped (their buffers go back to
    //= the pool as they drop).
    let Some(picture) = s.frames.try_iter().last() else {
        return;
    };
    s.geometry = Some(picture.geometry);
    let Some(image) = cg::frame_image(picture.frame) else {
        return;
    };
    // SAFETY: CALayer/CATransaction on the main thread; the layer retains
    // the CGImage, which we release when `image` drops.
    unsafe {
        let _: () = msg_send![class!(CATransaction), begin];
        let _: () = msg_send![class!(CATransaction), setDisableActions: true];
        let _: () = msg_send![&*s.image_layer, setContents: image.as_ptr().cast::<AnyObject>()];
        let _: () = msg_send![class!(CATransaction), commit];
    }
    //= Geometry may have changed with this picture.
    s.last_layout = None;
}

fn relayout(s: &mut UiState) {
    let Some(geom) = s.geometry else { return };
    let device = s.view.device_scale();
    let (w, h) = s.view.size();
    let Some(l) = geometry::layout(geom.view, device, w, h) else {
        return;
    };
    if s.last_layout == Some((l, geom.view)) {
        return;
    }
    s.last_layout = Some((l, geom.view));
    let frame = NSRect::new(NSPoint::new(l.x, l.y), NSSize::new(l.width, l.height));
    //= Integer scale: nearest keeps text crisp; otherwise filter smoothly.
    let filter = NSString::from_str(if l.is_integer() { "nearest" } else { "linear" });
    // SAFETY: CALayer setters on the main thread.
    unsafe {
        let _: () = msg_send![class!(CATransaction), begin];
        let _: () = msg_send![class!(CATransaction), setDisableActions: true];
        let _: () = msg_send![&*s.image_layer, setFrame: frame];
        let _: () = msg_send![&*s.image_layer, setMagnificationFilter: &*filter];
        let _: () =
            msg_send![&*s.image_layer, setMinificationFilter: &*NSString::from_str("linear")];
        let _: () = msg_send![class!(CATransaction), commit];
    }
}

/// Ask the server to match the window once its size has settled. As in the
/// GTK client, the gate compares the server's *view*, so a stream it chose
/// to send at reduced resolution is not mistaken for a mismatch.
fn maybe_resize(s: &mut UiState) {
    if s.geometry.is_none() {
        return;
    }
    let device = s.view.device_scale();
    let (w, h) = s.view.size();
    let Some(size) = geometry::resize_request(w, h, device) else {
        return;
    };
    let now = Instant::now();
    match s.resize_seen {
        Some((seen, d, _)) if seen == size && d == device => {}
        _ => {
            s.resize_seen = Some((size, device, now));
            return;
        }
    }
    let Some((_, _, since)) = s.resize_seen else {
        return;
    };
    if now.duration_since(since) < RESIZE_SETTLE {
        return;
    }
    let already = s.resize_sent == Some((size, device))
        || (size == s.server_view && s.resize_sent.is_some_and(|(_, d)| d == device));
    if already {
        return;
    }
    s.resize_sent = Some((size, device));
    tracing::debug!(?size, device, "resize request");
    s.send(ClientMsg::Resize {
        width: size.0,
        height: size.1,
        scale: device as f32,
    });
}

fn track_capture(s: &mut UiState) {
    let captured = s.captured();
    if captured && !s.was_captured {
        let how = s.hotkey.hotkey().describe();
        s.set_hint(format!("Keyboard captured — {how} to release"));
    }
    if !captured && s.was_captured {
        s.release_all_keys();
    }
    s.was_captured = captured;
}

fn update_subtitle(s: &mut UiState) {
    if s.hint
        .as_ref()
        .is_some_and(|(_, at)| at.elapsed() > HINT_FOR)
    {
        s.hint = None;
    }
    let text = match &s.hint {
        Some((h, _)) => h.clone(),
        None => s.line.clone(),
    };
    if text == s.shown_subtitle {
        return;
    }
    // SAFETY: -[NSWindow setSubtitle:] (macOS 11+) on the main thread.
    unsafe {
        let _: () = msg_send![&*s.window, setSubtitle: &*NSString::from_str(&text)];
    }
    s.shown_subtitle = text;
}

/// Build an NSCursor from a remote shape, or `None` for a shape that cannot
/// be shown (the caller then keeps the default).
fn make_cursor(shape: crate::session::CursorShape, device: f64) -> Option<Retained<AnyObject>> {
    let (w, h) = (shape.width, shape.height);
    if w == 0 || h == 0 || shape.bgra.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    if !(0..w as i32).contains(&shape.hot_x) || !(0..h as i32).contains(&shape.hot_y) {
        return None;
    }
    if shape.bgra.chunks_exact(4).all(|p| p[3] == 0) {
        //= Fully transparent: the remote hid its pointer.
        return None;
    }
    let device = if device > 0.0 { device } else { 1.0 };
    let (hot_x, hot_y) = (
        f64::from(shape.hot_x) / device,
        f64::from(shape.hot_y) / device,
    );
    let image = cg::cursor_image(shape.bgra, w, h)?;
    cursor_from_image(
        &image,
        f64::from(w) / device,
        f64::from(h) / device,
        hot_x,
        hot_y,
    )
}

/// A 1x1 transparent cursor, for headless screens.
pub fn hidden_cursor() -> Option<Retained<AnyObject>> {
    let image = cg::cursor_image(vec![0; 4], 1, 1)?;
    cursor_from_image(&image, 1.0, 1.0, 0.0, 0.0)
}

fn cursor_from_image(
    image: &cg::Image,
    width: f64,
    height: f64,
    hot_x: f64,
    hot_y: f64,
) -> Option<Retained<AnyObject>> {
    // SAFETY: NSImage/NSCursor initialisers on the main thread; NSImage
    // retains the CGImage, so `image` may drop afterwards.
    unsafe {
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSImage), alloc];
        let ns_image: Option<Retained<AnyObject>> = msg_send![
            alloc,
            initWithCGImage: image.as_ptr(),
            size: NSSize::new(width, height)
        ];
        let ns_image = ns_image?;
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSCursor), alloc];
        msg_send![alloc, initWithImage: &*ns_image, hotSpot: NSPoint::new(hot_x, hot_y)]
    }
}

/// Stop the worker and wait briefly for it to say goodbye to the server.
pub fn shutdown() {
    if let Some(mut s) = take() {
        s.release_all_keys();
        drop(s.input.take());
        let deadline = Instant::now() + Duration::from_millis(400);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match s.status.recv_timeout(left) {
                Ok(Status::Stopped) | Err(_) => break,
                Ok(_) => {}
            }
        }
        tracing::info!("gliff-mac stopped");
    }
}
