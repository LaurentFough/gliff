//! The AppKit front end: one window showing the remote screen.
//!
//! Threads: AppKit runs on the main thread; the session worker
//! (`crate::session`) runs on its own thread and talks to the UI through
//! three channels — pictures and statuses one way (polled by a 120 Hz
//! `NSTimer` in the common run-loop modes, so live resize keeps drawing),
//! input the other.
//!
//! Keyboard capture uses local `NSEvent` monitors rather than the view's
//! `keyDown:`: monitors also see Command chords (which AppKit would route
//! to the menu) and Command key-ups. System shortcuts (Command-Tab,
//! Mission Control, Spotlight) are taken by WindowServer before any app
//! and are not captured; see `docs/macos.md`.

mod cg;
mod ui;
mod view;

use std::ptr::NonNull;
use std::sync::mpsc;

use anyhow::Context;
use block2::RcBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, Sel};
use objc2::{class, define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};
use tokio::sync::mpsc::unbounded_channel;

use crate::hotkey::{HotkeyMatcher, ReleaseHotkey};
use crate::keys::{HeldKeys, KeyOptions};
use crate::session::{self, UiLink, WorkerConfig};
use ui::{KeyKind, UiState};
use view::GliffView;

//= Version of the AppKit front end.
pub const MAC_VERSION: &str = "0.1.0";

/// Initial window content size, in points.
const INITIAL_SIZE: (f64, f64) = (1280.0, 800.0);
/// The timer that moves pictures and statuses onto the screen.
const TICK_HZ: f64 = 120.0;

define_class!(
    // SAFETY: NSObject subclass with no ivars and no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GliffAppDelegate"]
    struct AppDelegate;

    impl AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _note: &AnyObject) {
            // SAFETY: NSApplication on the main thread.
            unsafe {
                let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
                let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
            }
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window(&self, _app: &AnyObject) -> bool {
            true
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _note: &AnyObject) {
            ui::shutdown();
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: -[NSObject init].
        unsafe { msg_send![super(this), init] }
    }
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// `[[cls alloc] init...]` helpers for classes we treat as `AnyObject`.
fn alloc(cls: &objc2::runtime::AnyClass) -> Allocated<AnyObject> {
    // SAFETY: +alloc on an NSObject subclass.
    unsafe { msg_send![cls, alloc] }
}

/// The standard app menu: About, Hide, Quit. When the keyboard is captured
/// the key monitor takes Command-Q for the remote; release it (or move the
/// pointer off the picture) to quit with the keyboard.
fn install_menu(app: &AnyObject) {
    let item = |title: &str, action: Option<Sel>, key: &str| -> Retained<AnyObject> {
        // SAFETY: NSMenuItem designated initialiser.
        unsafe {
            msg_send![
                alloc(class!(NSMenuItem)),
                initWithTitle: &*ns(title),
                action: action,
                keyEquivalent: &*ns(key)
            ]
        }
    };
    // SAFETY: NSMenu construction on the main thread.
    unsafe {
        let bar: Retained<AnyObject> = msg_send![alloc(class!(NSMenu)), initWithTitle: &*ns("")];
        let app_item = item("Gliff", None, "");
        let menu: Retained<AnyObject> =
            msg_send![alloc(class!(NSMenu)), initWithTitle: &*ns("Gliff")];
        let about = item("About Gliff", Some(sel!(orderFrontStandardAboutPanel:)), "");
        let hide = item("Hide Gliff", Some(sel!(hide:)), "h");
        let close = item("Close Window", Some(sel!(performClose:)), "w");
        let quit = item("Quit Gliff", Some(sel!(terminate:)), "q");
        for it in [&about, &hide, &close, &quit] {
            let _: () = msg_send![&*menu, addItem: &**it];
        }
        let _: () = msg_send![&*app_item, setSubmenu: &*menu];
        let _: () = msg_send![&*bar, addItem: &*app_item];
        let _: () = msg_send![app, setMainMenu: &*bar];
    }
}

fn make_window(mtm: MainThreadMarker, title: &str) -> (Retained<AnyObject>, Retained<GliffView>) {
    let rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(INITIAL_SIZE.0, INITIAL_SIZE.1),
    );
    //= Titled | Closable | Miniaturizable | Resizable.
    let style: usize = 1 | 2 | 4 | 8;
    //= NSBackingStoreBuffered.
    let backing: usize = 2;
    let view = GliffView::new(mtm, rect);
    // SAFETY: NSWindow setup on the main thread.
    let window: Retained<AnyObject> = unsafe {
        let window: Retained<AnyObject> = msg_send![
            alloc(class!(NSWindow)),
            initWithContentRect: rect,
            styleMask: style,
            backing: backing,
            defer: false
        ];
        //= Rust owns the window; AppKit must not free it on close.
        let _: () = msg_send![&*window, setReleasedWhenClosed: false];
        let _: () = msg_send![&*window, setTitle: &*ns(title)];
        let black: Retained<AnyObject> = msg_send![class!(NSColor), blackColor];
        let _: () = msg_send![&*window, setBackgroundColor: &*black];
        let _: () = msg_send![&*window, setContentMinSize: NSSize::new(320.0, 200.0)];
        let _: () = msg_send![&*window, setContentView: &*view];
        let _: () = msg_send![&*window, setAcceptsMouseMovedEvents: true];
        let _: () = msg_send![&*window, center];
        let _: () = msg_send![&*window, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        let _: bool = msg_send![&*window, makeFirstResponder: &*view];
        window
    };
    (window, view)
}

/// The picture's layer, added on top of the view's backing layer.
fn make_image_layer(view: &GliffView) -> anyhow::Result<Retained<AnyObject>> {
    // SAFETY: CALayer setup on the main thread; the view wants a layer.
    unsafe {
        let root: Option<Retained<AnyObject>> = msg_send![view, layer];
        let root = root.context("the view has no backing layer")?;
        let layer: Retained<AnyObject> = msg_send![class!(CALayer), layer];
        let _: () = msg_send![&*layer, setContentsGravity: &*ns("resize")];
        let _: () = msg_send![&*root, addSublayer: &*layer];
        Ok(layer)
    }
}

/// One local monitor per key-event kind; returning null swallows the event.
fn install_key_monitors() -> Vec<Retained<AnyObject>> {
    let mut monitors = Vec::new();
    for kind in [KeyKind::Down, KeyKind::Up, KeyKind::FlagsChanged] {
        let block = RcBlock::new(move |event: NonNull<AnyObject>| -> *mut AnyObject {
            // SAFETY: AppKit hands the handler a valid event.
            let e = unsafe { event.as_ref() };
            if ui::key_event(kind, e) {
                std::ptr::null_mut()
            } else {
                event.as_ptr()
            }
        });
        // SAFETY: +[NSEvent addLocalMonitorForEventsMatchingMask:handler:]
        // copies the block; the returned token keeps the monitor alive.
        let token: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                class!(NSEvent),
                addLocalMonitorForEventsMatchingMask: kind.mask(),
                handler: &*block
            ]
        };
        match token {
            Some(t) => monitors.push(t),
            None => tracing::warn!(?kind, "could not install a key monitor"),
        }
    }
    monitors
}

/// Schedule the UI tick in the common modes (default + event tracking).
fn install_timer() -> Retained<AnyObject> {
    let block = RcBlock::new(|_timer: NonNull<AnyObject>| ui::tick());
    // SAFETY: NSTimer/NSRunLoop on the main thread; the run loop retains
    // the timer and the timer copies the block.
    unsafe {
        let timer: Retained<AnyObject> = msg_send![
            class!(NSTimer),
            timerWithTimeInterval: 1.0 / TICK_HZ,
            repeats: true,
            block: &*block
        ];
        let run_loop: Retained<AnyObject> = msg_send![class!(NSRunLoop), mainRunLoop];
        let _: () = msg_send![
            &*run_loop,
            addTimer: &*timer,
            forMode: &*ns("kCFRunLoopCommonModes")
        ];
        timer
    }
}

/// Where the last host typed into the dialog is remembered.
fn last_host_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join("Library/Application Support/gliff-mac/last-host"))
}

/// Ask for `user@host` in a modal dialog, prefilled with the last answer.
/// `Ok(None)` when the user cancels.
pub fn ask_host() -> anyhow::Result<Option<String>> {
    MainThreadMarker::new().context("the dialog must run on the main thread")?;
    let last = last_host_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // SAFETY: NSApplication/NSAlert/NSTextField on the main thread.
    let answer: Option<String> = unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let _: bool = msg_send![&*app, setActivationPolicy: 0isize];
        let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
        let alert: Retained<AnyObject> = msg_send![alloc(class!(NSAlert)), init];
        let _: () = msg_send![&*alert, setMessageText: &*ns("Connect to a Hyprland machine")];
        let _: () = msg_send![
            &*alert,
            setInformativeText: &*ns("ssh destination running gliff-server, e.g. user@host")
        ];
        let _: Retained<AnyObject> = msg_send![&*alert, addButtonWithTitle: &*ns("Connect")];
        let _: Retained<AnyObject> = msg_send![&*alert, addButtonWithTitle: &*ns("Cancel")];
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 24.0));
        let field: Retained<AnyObject> =
            msg_send![alloc(class!(NSTextField)), initWithFrame: frame];
        let _: () = msg_send![&*field, setStringValue: &*ns(&last)];
        let _: () = msg_send![&*field, setPlaceholderString: &*ns("user@host")];
        let _: () = msg_send![&*alert, setAccessoryView: &*field];
        let window: Retained<AnyObject> = msg_send![&*alert, window];
        let _: () = msg_send![&*window, setInitialFirstResponder: &*field];
        //= NSAlertFirstButtonReturn.
        let response: isize = msg_send![&*alert, runModal];
        if response == 1000 {
            let value: Retained<NSString> = msg_send![&*field, stringValue];
            Some(value.to_string().trim().to_string())
        } else {
            None
        }
    };
    let Some(host) = answer.filter(|h| !h.is_empty()) else {
        return Ok(None);
    };
    if let Some(path) = last_host_path() {
        let saved = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, &host));
        if let Err(e) = saved {
            tracing::warn!(error = %e, path = %path.display(), "cannot remember the host");
        }
    }
    Ok(Some(host))
}

/// Open the window, start the worker, and run the app until quit.
pub fn run(cfg: WorkerConfig, keys: KeyOptions, hotkey: ReleaseHotkey) -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new().context("the window must run on the main thread")?;
    tracing::debug!(
        mac = MAC_VERSION,
        ui = ui::UI_VERSION,
        view = view::VIEW_VERSION,
        cg = cg::CG_VERSION,
        "front-end versions"
    );
    // SAFETY: NSApplication on the main thread.
    let app: Retained<AnyObject> = unsafe { msg_send![class!(NSApplication), sharedApplication] };
    // SAFETY: NSApplicationActivationPolicyRegular (0): a Dock icon and menu.
    unsafe {
        let _: bool = msg_send![&*app, setActivationPolicy: 0isize];
    }
    let delegate = AppDelegate::new(mtm);
    // SAFETY: the delegate outlives `run` (held below); NSApp keeps it weak.
    unsafe {
        let _: () = msg_send![&*app, setDelegate: &*delegate];
    }
    install_menu(&app);

    let host = cfg.endpoint.name().to_string();
    let (window, view) = make_window(mtm, &format!("{host} — Gliff"));
    let image_layer = make_image_layer(&view)?;

    let (frames_tx, frames_rx) = mpsc::sync_channel(2);
    let (status_tx, status_rx) = mpsc::channel();
    let (input_tx, input_rx) = unbounded_channel();
    let worker = session::spawn(
        cfg,
        UiLink {
            frames: frames_tx,
            status: status_tx,
            input: input_rx,
        },
    )
    .context("start the session worker")?;

    ui::install(UiState {
        input: Some(input_tx),
        frames: frames_rx,
        status: status_rx,
        keys,
        hotkey: HotkeyMatcher::new(hotkey),
        held: HeldKeys::default(),
        host,
        window,
        view,
        image_layer,
        geometry: None,
        headless: false,
        server_view: (0, 0),
        inside: false,
        released: false,
        was_captured: false,
        cursor: None,
        hidden_cursor: ui::hidden_cursor(),
        last_layout: None,
        resize_seen: None,
        resize_sent: None,
        line: "Connecting…".into(),
        hint: None,
        shown_subtitle: String::new(),
    });
    let _monitors = install_key_monitors();
    let _timer = install_timer();

    // SAFETY: -[NSApplication run] on the main thread; returns after
    // -terminate: only when the app is embedded, otherwise exits.
    unsafe {
        let _: () = msg_send![&*app, run];
    }
    //= Reached only if `run` returns (e.g. `stop:`); tidy up the same way.
    ui::shutdown();
    drop(delegate);
    let _ = worker.join();
    Ok(())
}
