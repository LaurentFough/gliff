//! gliff: a GTK4/libadwaita window that connects to a gliff server,
//! decodes the video, shows it, and forwards keyboard and pointer input.
//!
//! Decode runs on a worker thread (see `net`); this file is the UI. Decoded
//! frames arrive as dmabufs, which GTK imports as textures, so the UI has no
//! GPU code of its own; its one `unsafe` block hands GTK a dmabuf fd.

mod clipboard;
mod clipboard_ui;
mod keymap;
mod net;
mod paintable;
mod recent;
mod theme;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{channel, sync_channel, Receiver};
use std::time::{Duration, Instant};

use adw::prelude::*;
use clap::Parser;
use gliff_proto::{Axis, ClientMsg};
use gliff_transport::SshTarget;
use gliff_vk::DisplayFrame;
use gtk::gdk;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use net::Frame;
use net::{Endpoint, Picture, Status, Worker};
use recent::Config;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// A message plus optional trailing payload, sent from the UI to the worker.
type OutSender = UnboundedSender<clipboard::ToWorker>;

const APP_NAME: &str = "gliff";

#[derive(Parser, Clone)]
#[command(
    name = APP_NAME,
    version,
    about = "Remote-desktop a Hyprland session over ssh"
)]
struct Cli {
    /// `user@host` to ssh to and spawn gliff-server. Without it the window
    /// opens with a tab for each machine connected to before, or with the
    /// address bar focused when there are none.
    host: Option<String>,
    /// Disable clipboard sharing in both directions for every tab.
    #[arg(long)]
    no_clipboard: bool,
    /// Dev: connect directly to a `gliff-server --listen` address. Repeat it
    /// to open a tab for each.
    #[arg(long)]
    connect: Vec<String>,
    /// Remote gliff-server path.
    #[arg(long, default_value = "gliff-server")]
    server_bin: String,
    /// Create a private headless output on the remote, sized and scaled to
    /// this window, instead of mirroring the remote's focused screen.
    #[arg(long, conflicts_with = "output")]
    headless: bool,
    /// Mirror the named remote output (e.g. `DP-1`) instead of the focused one.
    #[arg(long)]
    output: Option<String>,
    /// Hotkey that releases captured shortcuts and hands the keyboard back to
    /// the local compositor. Forms: a chord like `shift+escape`, `ctrl+alt+q`
    /// or `super+escape`; `double-<key>` for a double-tap (e.g.
    /// `double-escape`); or `none` to disable. The screen recaptures when you
    /// click it again.
    #[arg(long, default_value = "shift+escape")]
    release_hotkey: String,
    /// Video pipeline: `gpu` (Vulkan compute + VA-API, falling back to the
    /// CPU when unavailable) or `cpu` (force OpenH264 on the CPU). Overrides
    /// the GLIFF_VIDEO environment variable.
    #[arg(long, value_parser = ["gpu", "cpu"])]
    video: Option<String>,
}

/// Everything the UI shares with its callbacks.
struct App {
    window: adw::ApplicationWindow,
    /// The picture, upcast; input controllers attach to it and we measure it.
    video: gtk::Widget,
    /// What the picture shows: the latest frame fitted to the window.
    frame: paintable::FramePaintable,
    stats: gtk::Label,
    /// Centred over the picture while the shown session is not streaming:
    /// connecting, reconnecting, an error, or no session at all.
    status: gtk::Label,
    /// A short-lived note near the bottom of the picture, such as how to
    /// release the captured shortcuts.
    hint: gtk::Label,
    /// Bumped by every hint, so a newer one is not hidden by an older timer.
    hint_generation: Cell<u64>,
    /// The capture hint shows once per window, not on every pointer entry.
    capture_hinted: Cell<bool>,
    /// A tab per machine, to the left of the address bar.
    tabs: gtk::Box,
    /// Expands into the address bar; hidden while the address bar shows.
    add_btn: gtk::Button,
    /// Holds the address bar, which shows only while adding a machine, or
    /// while there are no tabs at all.
    add_revealer: gtk::Revealer,
    /// The address bar: Enter connects to the machine typed.
    entry: gtk::Entry,
    transfers: Rc<clipboard_ui::TransferBars>,
    /// The last number given to a worker. Paste bars are keyed by it, so
    /// jobs of different connections never share a row.
    connections: Cell<u64>,
    /// Every running connection, in the order they were opened.
    sessions: RefCell<Vec<Rc<Session>>>,
    /// The session the window shows and sends input to.
    active: RefCell<Option<Rc<Session>>>,
    /// Offers the local clipboard to the shown session; set once the
    /// clipboard is watched.
    local_clipboard: RefCell<Option<clipboard_ui::LocalSelection>>,
    /// Size of the video widget in device pixels, rounded down to even.
    view_size: Cell<(u32, u32)>,
    view_scale: Cell<f64>,
    /// Evdev codes currently held on the remote, so they can all be released
    /// when the keyboard is handed back to the local compositor.
    pressed_keys: RefCell<BTreeSet<u32>>,
    /// Set by the release hotkey: the pointer is over the picture but the
    /// keyboard stays local until the pointer leaves, or the picture is
    /// clicked.
    released: Cell<bool>,
    config_path: PathBuf,
    cli: Cli,
    keymap: keymap::Keymap,
}

/// One connection to a machine. Every session keeps streaming while the
/// window shows another, so switching tabs is instant; only the shown one
/// gets input.
struct Session {
    /// The tab's name: `user@host`, or the address of a dev `--connect`.
    name: String,
    endpoint: Endpoint,
    input_tx: RefCell<Option<OutSender>>,
    /// Bumped by every `start_session` and by `stop_session`, so pollers and
    /// delayed reconnects of an earlier worker can tell they are stale.
    generation: Cell<u64>,
    /// The current worker's number from `App::connections`.
    conn: Cell<u64>,
    /// Consecutive failed connection attempts, reset on a successful connect.
    retries: Cell<u32>,
    /// Retained across slow reconnect attempts until a connection succeeds.
    last_error: RefCell<Option<String>>,
    /// Automatic reconnects have given up; selecting the tab tries again.
    gave_up: Cell<bool>,
    /// Recorded in the config on the first successful connect.
    remembered: Cell<bool>,
    /// The server is streaming: set by its Connected status, cleared when
    /// the connection drops.
    connected: Cell<bool>,
    /// The server streams a dedicated headless output: its frames carry the
    /// compositor-drawn cursor, so the local pointer is hidden over the video.
    headless: Cell<bool>,
    /// Size of the stream the server is sending, from the last decoded frame.
    stream_size: Cell<(u32, u32)>,
    /// The full-quality fit size the stream is drawn into; equals the stream
    /// size unless the server reduced the resolution. From the last decoded
    /// frame, so pointer mapping always matches the picture on screen.
    stream_view: Cell<(u32, u32)>,
    /// The view from the last StreamConfig (which may not be painted yet);
    /// only the resize gate reads it.
    server_view: Cell<(u32, u32)>,
    /// The server's frame-rate ceiling, for the stats overlay.
    fps_cap: Cell<u32>,
    /// The remote output's scale: pointer coordinates go in physical / scale.
    stream_scale: Cell<f32>,
    /// The size last asked of the server, so a pending resize is not repeated.
    resize_requested: Cell<(u32, u32)>,
    resize_scale: Cell<f64>,
    /// The output zoom last asked of the decoder; a new worker starts at 1.
    zoom: Cell<u32>,
    /// The latest frame and its view, painted when the tab is shown.
    last_frame: RefCell<Option<(gdk::Texture, (u32, u32))>>,
    cursor: RefCell<Option<gdk::Cursor>>,
    status: RefCell<String>,
    stats: RefCell<String>,
}

impl Session {
    fn reset_resize(&self) {
        self.resize_requested.set((0, 0));
        self.resize_scale.set(0.0);
    }

    fn new(endpoint: Endpoint) -> Self {
        Self {
            name: machine_name(&endpoint).to_string(),
            endpoint,
            input_tx: RefCell::new(None),
            generation: Cell::new(0),
            conn: Cell::new(0),
            retries: Cell::new(0),
            last_error: RefCell::new(None),
            gave_up: Cell::new(false),
            remembered: Cell::new(false),
            connected: Cell::new(false),
            headless: Cell::new(false),
            stream_size: Cell::new((0, 0)),
            stream_view: Cell::new((0, 0)),
            server_view: Cell::new((0, 0)),
            fps_cap: Cell::new(0),
            stream_scale: Cell::new(1.0),
            resize_requested: Cell::new((0, 0)),
            resize_scale: Cell::new(0.0),
            zoom: Cell::new(1),
            last_frame: RefCell::new(None),
            cursor: RefCell::new(None),
            status: RefCell::new(String::new()),
            stats: RefCell::new(String::new()),
        }
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    // GTK uses the program name as the Wayland app ID.
    glib::set_prgname(Some(APP_NAME));
    let app = adw::Application::builder().build();
    // The theme is per display, so it is set up once. The monitor lives in
    // this closure for the app's lifetime; dropping it would stop theme
    // updates.
    let theme_monitor = RefCell::new(None);
    app.connect_startup(move |_| {
        gtk::Window::set_default_icon_name(APP_NAME);
        *theme_monitor.borrow_mut() = theme::follow_omarchy_theme();
    });
    app.connect_activate(move |app| build_ui(app, &cli));
    // GTK owns argv parsing; we already parsed with clap, so pass none.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty)
}

/// The endpoints named on the command line, if any.
fn endpoints_from_cli(cli: &Cli) -> Vec<Endpoint> {
    if !cli.connect.is_empty() {
        return cli.connect.iter().cloned().map(Endpoint::Tcp).collect();
    }
    cli.host
        .iter()
        .map(|host| ssh_endpoint(cli, host))
        .collect()
}

fn ssh_endpoint(cli: &Cli, host: &str) -> Endpoint {
    let mut t = SshTarget::new(host.to_string());
    t.server_bin = cli.server_bin.clone();
    t.server_args = match (&cli.output, cli.headless) {
        (Some(name), _) => vec!["--output".into(), name.clone()],
        (None, true) => vec!["--headless".into()],
        (None, false) => vec!["--output".into(), "auto".into()],
    };
    Endpoint::Ssh(t)
}

/// The tab name for an endpoint.
fn machine_name(endpoint: &Endpoint) -> &str {
    match endpoint {
        Endpoint::Ssh(t) => t.host.as_str(),
        Endpoint::Tcp(addr) => addr.as_str(),
    }
}

/// Title in the `Document — App` form used by GNOME apps, so the machine
/// is what a task switcher shows first.
fn window_title(endpoint: &Endpoint) -> String {
    format!("{} — Gliff", machine_name(endpoint))
}

/// The one window: a header with a tab per machine and the address bar, the
/// remote screen (black until connected) with any status over it.
fn build_ui(app: &adw::Application, cli: &Cli) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Gliff")
        .default_width(1280)
        .default_height(760)
        .build();

    let header = adw::HeaderBar::builder()
        .show_title(false)
        // No window buttons: closing is the compositor's job, and a stray
        // click on an X in the middle of a remote session is a bad surprise.
        .decoration_layout("")
        .build();
    let entry = gtk::Entry::builder()
        .placeholder_text("user@host")
        .width_chars(32)
        .build();
    let connect_btn = gtk::Button::builder()
        .icon_name(CONNECT_ICON)
        .tooltip_text("Connect")
        .build();
    let address_bar = gtk::Box::builder().css_classes(["linked"]).build();
    address_bar.append(&entry);
    address_bar.append(&connect_btn);
    let add_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideRight)
        .child(&address_bar)
        .build();
    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Connect to another machine")
        .css_classes(["flat"])
        .build();
    let fullscreen_btn = gtk::ToggleButton::builder()
        .icon_name("view-fullscreen-symbolic")
        .build();
    let stats_btn = gtk::ToggleButton::builder()
        .icon_name("utilities-system-monitor-symbolic")
        .tooltip_text("Show stats")
        .build();
    let tabs = gtk::Box::builder().spacing(6).build();
    let tabs_scroller = gtk::ScrolledWindow::builder()
        .child(&tabs)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .build();
    header.pack_start(&tabs_scroller);
    header.pack_start(&add_btn);
    header.pack_start(&add_revealer);
    header.pack_end(&fullscreen_btn);
    header.pack_end(&stats_btn);

    let stats = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .css_classes(["stats"])
        .build();
    stats_btn
        .bind_property("active", &stats, "visible")
        .sync_create()
        .build();
    let status = gtk::Label::builder()
        .label("Not connected")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .can_target(false)
        .css_classes(["status-message"])
        .build();
    let hint = gtk::Label::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .visible(false)
        .can_target(false)
        .css_classes(["hint"])
        .build();

    // The video is a plain Picture given the whole allocation (Fill); the
    // paintable fits the frame using the surface device scale, see
    // paintable::layout.
    let picture = gtk::Picture::builder()
        .hexpand(true)
        .vexpand(true)
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Fill)
        .css_classes(["video"])
        .build();
    let frame = paintable::FramePaintable::default();
    picture.set_paintable(Some(&frame));
    let video: gtk::Widget = picture.clone().upcast();

    let transfers = Rc::new(clipboard_ui::TransferBars::new());
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&video));
    overlay.add_overlay(&stats);
    overlay.add_overlay(transfers.widget());
    overlay.add_overlay(&status);
    overlay.add_overlay(&hint);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&overlay);
    window.set_content(Some(&content));
    install_fullscreen_bars(
        &window,
        &content,
        &overlay,
        &header,
        &fullscreen_btn,
        &entry,
    );

    let ui = Rc::new(App {
        window: window.clone(),
        video: video.clone(),
        frame,
        stats: stats.clone(),
        status: status.clone(),
        hint,
        hint_generation: Cell::new(0),
        capture_hinted: Cell::new(false),
        tabs,
        add_btn,
        add_revealer,
        entry: entry.clone(),
        transfers,
        connections: Cell::new(0),
        sessions: RefCell::new(Vec::new()),
        active: RefCell::new(None),
        local_clipboard: RefCell::new(None),
        view_size: Cell::new((0, 0)),
        view_scale: Cell::new(1.0),
        pressed_keys: RefCell::new(BTreeSet::new()),
        released: Cell::new(false),
        config_path: Config::default_path(),
        cli: cli.clone(),
        keymap: keymap::watch(),
    });

    let hotkey = ReleaseHotkey::parse(&cli.release_hotkey).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "invalid --release-hotkey; shortcut release disabled");
        ReleaseHotkey::None
    });
    install_input_handlers(&ui, &video, &window, hotkey);
    install_resize_handler(&ui);
    install_address_bar(&ui, &connect_btn);
    refresh_tabs(&ui);

    // Fullscreen toggle.
    {
        let window = window.clone();
        fullscreen_btn.connect_toggled(move |b| {
            if b.is_active() {
                window.fullscreen();
            } else {
                window.unfullscreen();
            }
        });
    }

    let css = gtk::CssProvider::new();
    css.load_from_string(concat!(
        ".video { background: #000; }",
        ".status-message { color: rgba(255,255,255,0.7); font-size: 1.15em; margin: 24px; }",
        ".hint { background: rgba(0,0,0,0.7); color: #fff; padding: 6px 14px; border-radius: 999px; margin-bottom: 24px; }",
        ".stats { background: rgba(0,0,0,0.6); color: #fff; padding: 6px; margin: 6px; border-radius: 6px; font-family: monospace; }",
        ".transfers { margin: 6px; }",
        ".transfer { background: rgba(0,0,0,0.7); color: #fff; padding: 6px 8px; border-radius: 6px; }",
        // A tab is one pill: the name and its stop/forget icon carry no
        // backgrounds of their own, only the icon brightens under the pointer.
        ".machine-tab { border-radius: 8px; }",
        ".machine-tab:hover { background: alpha(currentColor, 0.07); }",
        ".machine-tab.active { background: alpha(currentColor, 0.14); }",
        ".machine-tab > .tab-label { padding: 5px 2px 5px 12px; font-weight: bold; }",
        ".machine-tab > button { background: none; border: none; outline: none; box-shadow: none; min-height: 0; }",
        ".machine-tab > button.tab-action { padding: 0; min-width: 22px; min-height: 22px; margin-right: 4px; color: alpha(currentColor, 0.55); -gtk-icon-size: 12px; }",
        ".machine-tab > button.tab-action:hover { color: currentColor; }",
        ".status-dot { min-width: 6px; min-height: 6px; border-radius: 50%; border: 1.5px solid alpha(currentColor, 0.6); }",
        ".status-dot.connected { background: var(--success-color); border-color: var(--success-color); }",
    ));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // Offer every local clipboard change to the shown server; our own proxy
    // for a server's selection does not count as a change.
    {
        let running = Rc::downgrade(&ui);
        let shown = Rc::downgrade(&ui);
        let watch = clipboard_ui::watch_local(
            move || {
                running
                    .upgrade()
                    .map(|ui| running_inputs(&ui.sessions.borrow(), ui.cli.no_clipboard))
                    .unwrap_or_default()
            },
            move || {
                shown
                    .upgrade()
                    .map(|ui| {
                        local_offer_targets(
                            &ui.sessions.borrow(),
                            active(&ui).as_ref(),
                            ui.cli.no_clipboard,
                        )
                    })
                    .unwrap_or_default()
            },
        );
        *ui.local_clipboard.borrow_mut() = watch;
    }

    window.present();
    // Reconnect the machines that were connected when the window last
    // closed, then any named on the command line, which is shown first.
    let config = Config::load(&ui.config_path);
    let from_cli = endpoints_from_cli(cli);
    for machine in &config.open {
        open_session(&ui, ssh_endpoint(cli, machine));
    }
    for endpoint in from_cli.iter().cloned() {
        if find_session(&ui, machine_name(&endpoint)).is_none() {
            open_session(&ui, endpoint);
        }
    }
    // The address bar showed for the empty tabs; with sessions open it
    // folds into the +.
    show_address_bar(&ui, false);
    let first = from_cli
        .first()
        .map(|e| machine_name(e).to_string())
        .or(config.shown)
        .and_then(|name| find_session(&ui, &name))
        .or_else(|| ui.sessions.borrow().first().cloned());
    match first {
        Some(first) => show_session(&ui, &first),
        None if ui.tabs.first_child().is_none() => {
            entry.grab_focus();
        }
        None => {}
    }
}

const CONNECT_ICON: &str = "go-next-symbolic";
const STOP_ICON: &str = "media-playback-stop-symbolic";
const FORGET_ICON: &str = "window-close-symbolic";

/// Wire the address bar: the + expands it, Enter connects to what was
/// typed, and Escape, or leaving it empty, folds it back into the +.
fn install_address_bar(ui: &Rc<App>, connect_btn: &gtk::Button) {
    {
        let ui = ui.clone();
        ui.entry
            .clone()
            .connect_activate(move |_| connect_from_address_bar(&ui));
    }
    {
        let ui = ui.clone();
        connect_btn.connect_clicked(move |_| connect_from_address_bar(&ui));
    }
    {
        let ui = ui.clone();
        ui.add_btn.clone().connect_clicked(move |_| {
            show_address_bar(&ui, true);
            ui.entry.grab_focus();
        });
    }

    let focus = gtk::EventControllerFocus::new();
    {
        let ui = ui.clone();
        // Focus may be moving to the connect button, so decide once the new
        // focus is known.
        focus.connect_leave(move |_| {
            let ui = ui.clone();
            glib::idle_add_local_once(move || {
                let in_bar = gtk::prelude::GtkWindowExt::focus(&ui.window)
                    .is_some_and(|w| w.is_ancestor(&ui.add_revealer));
                if !in_bar && ui.entry.text().trim().is_empty() {
                    show_address_bar(&ui, false);
                }
            });
        });
    }
    ui.entry.add_controller(focus);

    let keys = gtk::EventControllerKey::new();
    {
        let ui = ui.clone();
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gdk::Key::Escape {
                cancel_address_edit(&ui);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    ui.entry.add_controller(keys);
}

/// Show the address bar in place of the +, or fold it back. With no tabs
/// the address bar is all there is, so it stays.
fn show_address_bar(ui: &App, show: bool) {
    let show = show || ui.tabs.first_child().is_none();
    ui.add_revealer.set_reveal_child(show);
    ui.add_btn.set_visible(!show);
    // A folded address bar must not keep the focus GTK gave it when the
    // window opened, or hovering the picture would never capture.
    if !show && address_bar_in_use(&ui.window, &ui.entry) {
        gtk::prelude::GtkWindowExt::set_focus(&ui.window, gtk::Widget::NONE);
    }
}

/// Connect to the machine in the address bar, or switch to its tab when it
/// is already connected.
fn connect_from_address_bar(ui: &Rc<App>) {
    let machine = ui.entry.text().trim().to_string();
    if machine.is_empty() {
        return;
    }
    ui.entry.set_text("");
    match find_session(ui, &machine) {
        Some(s) => show_session(ui, &s),
        None => open_session(ui, ssh_endpoint(&ui.cli, &machine)),
    }
    show_address_bar(ui, false);
    if active(ui).is_some() {
        ui.video.grab_focus();
    }
}

/// Give up the edit: empty the address bar, fold it back into the +, and
/// hand focus back to the remote screen when there is one.
fn cancel_address_edit(ui: &App) {
    ui.entry.set_text("");
    show_address_bar(ui, false);
    if active(ui).is_some() {
        ui.video.grab_focus();
    } else if ui.add_revealer.reveals_child() {
        ui.entry.grab_focus();
    } else {
        gtk::prelude::GtkWindowExt::set_focus(&ui.window, gtk::Widget::NONE);
    }
}

fn active(ui: &App) -> Option<Rc<Session>> {
    ui.active.borrow().clone()
}

fn is_active(ui: &App, s: &Rc<Session>) -> bool {
    ui.active
        .borrow()
        .as_ref()
        .is_some_and(|a| Rc::ptr_eq(a, s))
}

/// Every running session's input. A local clipboard change revokes the
/// offer in all of them. Only the shown session can hold one, since a switch
/// withdraws it, but revoking every session keeps that from resting on the
/// switch alone.
fn running_inputs(sessions: &[Rc<Session>], no_clipboard: bool) -> Vec<OutSender> {
    if no_clipboard {
        return Vec::new();
    }
    sessions
        .iter()
        .filter_map(|s| s.input_tx.borrow().clone())
        .collect()
}

/// Who the local clipboard is offered to: only the shown session, so a host
/// in a background tab cannot read what is copied while it is hidden.
fn local_offer_targets(
    sessions: &[Rc<Session>],
    active: Option<&Rc<Session>>,
    no_clipboard: bool,
) -> Vec<OutSender> {
    if no_clipboard {
        return Vec::new();
    }
    sessions
        .iter()
        .filter(|s| active.is_some_and(|a| Rc::ptr_eq(a, s)))
        .filter_map(|s| s.input_tx.borrow().clone())
        .collect()
}

/// Withdraw the local clipboard from a session that is no longer shown. The
/// revocation refuses the host's further requests and aborts what it is
/// being served. The empty offer after it tells the host to drop the
/// selection it advertised for us, so its applications stop seeing one.
fn revoke_local_offer(previous: Option<&Rc<Session>>, no_clipboard: bool) {
    if no_clipboard {
        return;
    }
    if let Some(tx) = previous.and_then(|s| s.input_tx.borrow().clone()) {
        let _ = tx.send(clipboard::ToWorker::RevokeLocal);
        // Nothing can be read from an empty offer, so its generation is moot.
        let _ = tx.send(clipboard::ToWorker::LocalOffer {
            mime_types: Vec::new(),
            files: Default::default(),
            generation: 0,
        });
    }
}

/// Move the local clipboard to the session just shown: withdraw it from the
/// one shown before, then offer the current selection again, which reaches
/// only the shown session. The withdrawal is queued at once, and the new offer
/// only after the selection has been read.
fn move_local_offer(
    previous: Option<&Rc<Session>>,
    no_clipboard: bool,
    watch: Option<&clipboard_ui::LocalSelection>,
) {
    if no_clipboard {
        return;
    }
    revoke_local_offer(previous, no_clipboard);
    if let Some(watch) = watch {
        watch.reoffer();
    }
}

/// Whether a server's request may read the local clipboard now. Its worker
/// may have accepted the request before a tab switch or a new copy, so the
/// session must still be shown and its offer still the latest.
fn may_read_local(
    active: Option<&Rc<Session>>,
    s: &Rc<Session>,
    no_clipboard: bool,
    generation: u64,
    watch: Option<&clipboard_ui::LocalSelection>,
) -> bool {
    !no_clipboard
        && active.is_some_and(|a| Rc::ptr_eq(a, s))
        && watch.is_some_and(|w| w.generation() == generation)
}

/// Serve a server's read of the local clipboard. `shown` gives the shown
/// session and the clipboard watcher when the read starts, which may be
/// after a tab switch or a new copy.
fn serve_local_read(
    s: &Rc<Session>,
    no_clipboard: bool,
    mime_type: String,
    generation: u64,
    reply: tokio::sync::mpsc::Sender<std::io::Result<bytes::Bytes>>,
    shown: impl Fn() -> (Option<Rc<Session>>, Option<clipboard_ui::LocalSelection>) + 'static,
) {
    if no_clipboard {
        return;
    }
    let s = s.clone();
    clipboard_ui::read_local(mime_type, reply, move || {
        let (active, watch) = shown();
        may_read_local(
            active.as_ref(),
            &s,
            no_clipboard,
            generation,
            watch.as_ref(),
        )
    });
}

fn find_session(ui: &App, name: &str) -> Option<Rc<Session>> {
    ui.sessions
        .borrow()
        .iter()
        .find(|s| s.name == name)
        .cloned()
}

/// The tabs, in order: the remembered machines as arranged, then any session
/// not remembered yet (a first connect still in progress, or a dev
/// `--connect`).
fn tab_names(ui: &App) -> Vec<String> {
    let mut names = Config::load(&ui.config_path).machines;
    for s in ui.sessions.borrow().iter() {
        if !names.contains(&s.name) {
            names.push(s.name.clone());
        }
    }
    names
}

/// Rebuild the tabs from the config and the running sessions.
fn refresh_tabs(ui: &Rc<App>) {
    while let Some(tab) = ui.tabs.first_child() {
        ui.tabs.remove(&tab);
    }
    for name in tab_names(ui) {
        ui.tabs.append(&machine_tab(ui, &name));
    }
    // An empty scroller still claims its minimum width, which would push
    // the address bar away from where the first tab starts.
    let has_tabs = ui.tabs.first_child().is_some();
    if let Some(scroller) = ui.tabs.ancestor(gtk::ScrolledWindow::static_type()) {
        scroller.set_visible(has_tabs);
    }
    if !has_tabs {
        show_address_bar(ui, true);
    }
}

/// A machine's tab: its name, which shows that machine, and a button that
/// appears on hover: stop for a running session, forget for the rest. Tabs
/// can be dragged into a new order.
fn machine_tab(ui: &Rc<App>, name: &str) -> gtk::Box {
    let session = find_session(ui, name);
    let running = session.is_some();
    // The name is a plain label and the whole tab takes the click, so no
    // part of a tab takes keyboard focus and grows a focus ring.
    let label = gtk::Label::builder()
        .label(name)
        .css_classes(["tab-label"])
        .build();
    if !running {
        label.add_css_class("dim-label");
    }
    // The slot on the right shows whether the machine is connected, and
    // turns into its button while the pointer is on it.
    let connected = session.as_ref().is_some_and(|s| s.connected.get());
    let status_tip = if connected {
        "Connected"
    } else {
        "Not connected"
    };
    let action_tip = if running { "Disconnect" } else { "Forget" };
    let dot = gtk::Box::builder()
        .css_classes(["status-dot"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    if connected {
        dot.add_css_class("connected");
    }
    let slot = gtk::Stack::new();
    slot.add_named(&dot, Some("status"));
    slot.add_named(
        &gtk::Image::from_icon_name(if running { STOP_ICON } else { FORGET_ICON }),
        Some("action"),
    );
    let action = gtk::Button::builder()
        .child(&slot)
        .tooltip_text(status_tip)
        .css_classes(["tab-action"])
        .valign(gtk::Align::Center)
        .focusable(false)
        .build();
    let tab = gtk::Box::builder().css_classes(["machine-tab"]).build();
    if session.as_ref().is_some_and(|s| is_active(ui, s)) {
        tab.add_css_class("active");
    }
    tab.append(&label);
    tab.append(&action);

    // The dot turns into its button only under the pointer, so a click on
    // it always lands on what it shows.
    let hover = gtk::EventControllerMotion::new();
    {
        let slot = slot.clone();
        hover.connect_enter(move |c, _, _| {
            slot.set_visible_child_name("action");
            if let Some(button) = c.widget() {
                button.set_tooltip_text(Some(action_tip));
            }
        });
    }
    {
        let slot = slot.clone();
        hover.connect_leave(move |c| {
            slot.set_visible_child_name("status");
            if let Some(button) = c.widget() {
                button.set_tooltip_text(Some(status_tip));
            }
        });
    }
    action.add_controller(hover);

    // The handlers rebuild the tabs, so they run once the click is done
    // with the widget that received it. The action button claims its own
    // clicks, so this sees only the rest of the tab.
    let click = gtk::GestureClick::builder().button(1).build();
    {
        let ui = ui.clone();
        let name = name.to_string();
        click.connect_released(move |_, _, _, _| {
            let (ui, name) = (ui.clone(), name.clone());
            glib::idle_add_local_once(move || select_tab(&ui, &name));
        });
    }
    tab.add_controller(click);
    {
        let ui = ui.clone();
        let name = name.to_string();
        action.connect_clicked(move |_| {
            let (ui, name) = (ui.clone(), name.clone());
            glib::idle_add_local_once(move || {
                if running {
                    stop_session(&ui, &name);
                } else {
                    forget_machine(&ui, &name);
                }
            });
        });
    }

    // Drag to reorder. The source runs in the capture phase so a drag
    // starting on the name wins over the button's click and the header's
    // window move.
    let drag = gtk::DragSource::builder()
        .actions(gdk::DragAction::MOVE)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    {
        let name = name.to_string();
        drag.connect_prepare(move |source, x, y| {
            if let Some(tab) = source.widget() {
                let icon = gtk::WidgetPaintable::new(Some(&tab));
                source.set_icon(Some(&icon), x as i32, y as i32);
            }
            Some(gdk::ContentProvider::for_value(&name.to_value()))
        });
    }
    tab.add_controller(drag);

    let drop = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
    {
        let ui = ui.clone();
        let name = name.to_string();
        drop.connect_drop(move |target, value, x, _| {
            let Ok(dragged) = value.get::<String>() else {
                return false;
            };
            let after = target.widget().is_some_and(|w| x > w.width() as f64 / 2.0);
            let (ui, name) = (ui.clone(), name.clone());
            glib::idle_add_local_once(move || move_tab(&ui, &dragged, &name, after));
            true
        });
    }
    tab.add_controller(drop);
    tab
}

/// Show a machine's tab: switch to its running session, retrying one whose
/// reconnects gave up, or connect to it.
fn select_tab(ui: &Rc<App>, name: &str) {
    match find_session(ui, name) {
        Some(s) => {
            if s.gave_up.get() {
                s.retries.set(0);
                s.gave_up.set(false);
                start_session(ui.clone(), s.clone());
            }
            show_session(ui, &s);
        }
        None => open_session(ui, ssh_endpoint(&ui.cli, name)),
    }
}

/// Connect to `endpoint` in a new session and show it.
fn open_session(ui: &Rc<App>, endpoint: Endpoint) {
    let s = Rc::new(Session::new(endpoint));
    ui.sessions.borrow_mut().push(s.clone());
    start_session(ui.clone(), s.clone());
    show_session(ui, &s);
}

/// Make `s` the session the window shows and sends input to.
fn show_session(ui: &Rc<App>, s: &Rc<Session>) {
    if !is_active(ui, s) {
        release_pressed_keys(ui);
        let previous = ui.active.replace(Some(s.clone()));
        move_local_offer(
            previous.as_ref(),
            ui.cli.no_clipboard,
            ui.local_clipboard.borrow().as_ref(),
        );
        if matches!(s.endpoint, Endpoint::Ssh(_)) {
            let mut config = Config::load(&ui.config_path);
            config.shown = Some(s.name.clone());
            save_config(ui, &config);
        }
    }
    ui.window.set_title(Some(&window_title(&s.endpoint)));
    show_status(ui, &s.status.borrow());
    ui.stats.set_text(&s.stats.borrow());
    match &*s.last_frame.borrow() {
        Some((texture, view)) => ui.frame.set_frame(texture.clone(), device_scale(ui), *view),
        None => ui.frame.clear(),
    }
    ui.video.set_cursor(s.cursor.borrow().as_ref());
    // A background session kept the size the window had when it was last
    // shown.
    request_resize(ui);
    request_zoom(ui);
    refresh_tabs(ui);
}

/// Show no session: a black screen, as on a fresh window.
fn show_nothing(ui: &Rc<App>) {
    release_pressed_keys(ui);
    let previous = ui.active.take();
    revoke_local_offer(previous.as_ref(), ui.cli.no_clipboard);
    ui.window.set_title(Some("Gliff"));
    show_status(ui, "Not connected");
    ui.stats.set_text("");
    ui.frame.clear();
    ui.video.set_cursor(None);
    refresh_tabs(ui);
}

/// End a machine's session. When it was the one shown, the window moves to
/// the nearest running tab, to the right first.
fn stop_session(ui: &Rc<App>, name: &str) {
    let Some(s) = find_session(ui, name) else {
        return;
    };
    let was_active = is_active(ui, &s);
    if was_active {
        release_pressed_keys(ui);
    }
    // Bumping the generation stops its pollers and pending reconnects, and
    // dropping the sender closes the worker's input, which ends it.
    s.generation.set(s.generation.get() + 1);
    s.input_tx.borrow_mut().take();
    ui.transfers.clear_connection(s.conn.get());
    s.last_frame.borrow_mut().take();
    // A machine stopped by hand stays stopped when the window reopens.
    if matches!(s.endpoint, Endpoint::Ssh(_)) {
        let mut config = Config::load(&ui.config_path);
        config.set_open(&s.name, false);
        save_config(ui, &config);
    }
    let names = tab_names(ui);
    ui.sessions.borrow_mut().retain(|o| !Rc::ptr_eq(o, &s));
    if !was_active {
        refresh_tabs(ui);
        return;
    }
    let at = names.iter().position(|n| *n == s.name).unwrap_or(0);
    let next = names[at + 1..]
        .iter()
        .chain(names[..at].iter().rev())
        .find_map(|n| find_session(ui, n));
    match next {
        Some(next) => show_session(ui, &next),
        None => show_nothing(ui),
    }
}

/// Drop a machine that is not running from the remembered list.
fn forget_machine(ui: &Rc<App>, name: &str) {
    let mut config = Config::load(&ui.config_path);
    config.forget(name);
    save_config(ui, &config);
    refresh_tabs(ui);
}

/// Move the dragged tab next to the one it was dropped on. Only remembered
/// machines have a place to keep.
fn move_tab(ui: &Rc<App>, dragged: &str, target: &str, after: bool) {
    let mut config = Config::load(&ui.config_path);
    if config.move_machine(dragged, target, after) {
        save_config(ui, &config);
        refresh_tabs(ui);
    }
}

fn save_config(ui: &App, config: &Config) {
    if let Err(e) = config.save(&ui.config_path) {
        tracing::warn!(path = %ui.config_path.display(), error = %e, "cannot save config");
    }
}

/// Set a session's status, showing it when the session is. A streaming
/// session has none.
fn set_status(ui: &App, s: &Rc<Session>, text: &str) {
    *s.status.borrow_mut() = text.to_string();
    if is_active(ui, s) {
        show_status(ui, text);
    }
}

fn show_status(ui: &App, text: &str) {
    ui.status.set_text(text);
    ui.status.set_visible(!text.is_empty());
}

/// Show a note near the bottom of the picture for a few seconds.
fn show_hint(ui: &Rc<App>, text: &str) {
    let generation = ui.hint_generation.get() + 1;
    ui.hint_generation.set(generation);
    ui.hint.set_text(text);
    ui.hint.set_visible(true);
    let ui = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(2500), move || {
        if ui.hint_generation.get() == generation {
            ui.hint.set_visible(false);
        }
    });
}

fn set_stats(ui: &App, s: &Rc<Session>, text: &str) {
    *s.stats.borrow_mut() = text.to_string();
    if is_active(ui, s) {
        ui.stats.set_text(text);
    }
}

/// Go back to a black screen with no stream geometry.
fn show_disconnected(ui: &Rc<App>, s: &Rc<Session>) {
    if s.connected.replace(false) {
        refresh_tabs(ui);
    }
    s.last_frame.borrow_mut().take();
    s.stream_size.set((0, 0));
    s.stream_view.set((0, 0));
    s.server_view.set((0, 0));
    s.cursor.borrow_mut().take();
    ui.transfers.clear_connection(s.conn.get());
    set_stats(ui, s, "");
    if is_active(ui, s) {
        ui.frame.clear();
        ui.video.set_cursor(None);
    }
}

/// Start a worker for the session. Each call is a new generation; pollers
/// and reconnects from an older one stop themselves.
fn start_session(ui: Rc<App>, s: Rc<Session>) {
    let (frame_tx, frame_rx) = sync_channel::<Picture>(2);
    let (status_tx, status_rx) = channel::<Status>();
    let (input_tx, input_rx) = unbounded_channel::<clipboard::ToWorker>();
    // Replacing the sender closes the old worker's input, which ends it.
    *s.input_tx.borrow_mut() = Some(input_tx);
    s.zoom.set(1);
    s.reset_resize();
    let generation = s.generation.get() + 1;
    s.generation.set(generation);
    ui.transfers.clear_connection(s.conn.get());
    let conn = ui.connections.get() + 1;
    ui.connections.set(conn);
    s.conn.set(conn);
    set_status(
        &ui,
        &s,
        &connecting_status(s.last_error.borrow().as_deref()),
    );

    let endpoint = s.endpoint.clone();
    let video = gliff_sw::VideoMode::resolve(ui.cli.video.as_deref());
    let keymap = ui.keymap.clone();
    std::thread::Builder::new()
        .name("gliff-net".into())
        .spawn(move || {
            Worker {
                endpoint,
                video,
                frames: frame_tx,
                status: status_tx,
                input: input_rx,
                keymap,
            }
            .run();
        })
        .expect("spawn network thread");

    poll_frames(ui.clone(), s.clone(), frame_rx, generation);
    poll_status(ui, s, status_rx, generation);
}

const MAX_RETRIES: u32 = 5;

/// Add the session's machine to the remembered list, and to the ones to
/// reconnect when the window opens again, once per session. The connected
/// status repeats on every stream reconfigure, such as a resize.
fn remember_machine(ui: &Rc<App>, s: &Session) {
    if !matches!(s.endpoint, Endpoint::Ssh(_)) || s.remembered.replace(true) {
        return;
    }
    let mut config = Config::load(&ui.config_path);
    config.remember(&s.name);
    config.set_open(&s.name, true);
    save_config(ui, &config);
    refresh_tabs(ui);
}

/// Schedule a reconnect after a short delay, unless we have exhausted retries.
/// The status keeps the error that ended the connection, if any, so it stays
/// on screen while the reconnects run.
fn schedule_reconnect(ui: Rc<App>, s: Rc<Session>, generation: u64, error: Option<&str>) {
    let n = s.retries.get() + 1;
    s.retries.set(n);
    if let Some(error) = error {
        *s.last_error.borrow_mut() = Some(error.to_owned());
    }
    set_status(
        &ui,
        &s,
        &reconnect_status(s.last_error.borrow().as_deref(), n),
    );
    if n > MAX_RETRIES {
        s.gave_up.set(true);
        return;
    }
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        if s.generation.get() == generation {
            start_session(ui, s);
        }
    });
}

/// Keep the latest failure visible while a new connection attempt runs.
fn connecting_status(error: Option<&str>) -> String {
    match error {
        Some(e) => format!("Error: {e}\nConnecting…"),
        None => "Connecting…".into(),
    }
}

/// The status for reconnect `attempt`, under the error that ended the
/// connection, if any.
fn reconnect_status(error: Option<&str>, attempt: u32) -> String {
    let next = if attempt > MAX_RETRIES {
        "Disconnected — click the tab to reconnect".to_string()
    } else {
        format!("Reconnecting… (attempt {attempt})")
    };
    match error {
        Some(e) => format!("Error: {e}\n{next}"),
        None => next,
    }
}

/// Pull decoded frames on the GTK main loop, latest-wins. The shown session
/// paints them; every session keeps its latest for when its tab is shown.
fn poll_frames(ui: Rc<App>, s: Rc<Session>, rx: Receiver<Picture>, generation: u64) {
    glib::timeout_add_local(Duration::from_millis(8), move || {
        if s.generation.get() != generation {
            return glib::ControlFlow::Break;
        }
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(f) => latest = Some(f),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // The worker ended; stop this per-session timer so it does not
                // accumulate across reconnects.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return glib::ControlFlow::Break
                }
            }
        }
        if let Some(p) = latest {
            match frame_texture(p.frame) {
                Ok(texture) => {
                    // Geometry follows the frame that really paints, so the
                    // pointer never maps against a picture that is not on
                    // screen (a newer config, or a failed import).
                    s.stream_size.set(p.stream);
                    s.stream_view.set(p.view);
                    s.stream_scale.set((p.scale_milli.max(1) as f32) / 1000.0);
                    if is_active(&ui, &s) {
                        ui.frame
                            .set_frame(texture.clone(), device_scale(&ui), p.view);
                    }
                    *s.last_frame.borrow_mut() = Some((texture, p.view));
                }
                Err(e) => tracing::warn!(error = %e, "frame texture import failed"),
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Wrap a decoded frame as a GDK texture: a dmabuf import for the GPU tier,
/// a plain memory texture for the CPU tier.
fn frame_texture(f: Frame) -> Result<gdk::Texture, glib::Error> {
    match f {
        Frame::Dmabuf(f) => dmabuf_texture(f),
        Frame::Bgra(f) => {
            let bytes = glib::Bytes::from_owned(f.pixels);
            Ok(gdk::MemoryTexture::new(
                f.width as i32,
                f.height as i32,
                gdk::MemoryFormat::B8g8r8x8,
                &bytes,
                f.width as usize * 4,
            )
            .upcast())
        }
    }
}

/// Wrap a decoded frame's dmabuf as a GDK texture. The frame (and its fd)
/// lives until GTK releases the texture, which returns the image to the
/// decoder's ring.
fn dmabuf_texture(f: DisplayFrame) -> Result<gdk::Texture, glib::Error> {
    let display = gdk::Display::default()
        .ok_or_else(|| glib::Error::new(glib::FileError::Failed, "no display"))?;
    let builder = gdk::DmabufTextureBuilder::new()
        .set_display(&display)
        .set_width(f.width)
        .set_height(f.height)
        .set_fourcc(f.fourcc as u32)
        .set_modifier(f.modifier)
        .set_n_planes(1)
        .set_offset(0, f.offset)
        .set_stride(0, f.stride)
        .set_premultiplied(false);
    // SAFETY: the fd is a dmabuf describing exactly one linear plane of the
    // stated size, stride and format, and the frame that owns it is kept
    // alive by the release closure until GTK is done with the texture.
    unsafe {
        builder
            .set_fd(0, f.fd.as_raw_fd())
            .build_with_release_func(move || drop(f))
    }
}

fn poll_status(ui: Rc<App>, s: Rc<Session>, rx: Receiver<Status>, generation: u64) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if s.generation.get() != generation {
            return glib::ControlFlow::Break;
        }
        while let Ok(status) = rx.try_recv() {
            match status {
                // Pointer-mapping geometry (stream_size/view/scale) is not
                // read from the config: it follows each decoded frame, so a
                // new config never remaps clicks against the old picture.
                Status::Connected {
                    video,
                    view_width,
                    view_height,
                    fps_cap,
                    headless,
                } => {
                    s.server_view.set((view_width, view_height));
                    s.fps_cap.set(fps_cap);
                    s.headless.set(headless);
                    s.resize_requested.set((0, 0));
                    s.last_error.borrow_mut().take();
                    s.retries.set(0);
                    s.gave_up.set(false);
                    if !s.connected.replace(true) {
                        refresh_tabs(&ui);
                    }
                    set_status(&ui, &s, "");
                    // Visible before the first per-second stats arrive.
                    set_stats(&ui, &s, &video);
                    remember_machine(&ui, &s);
                    // A fresh server starts at its own default size; a
                    // reconnect must bring it back to the window. The gate
                    // compares against the view, so a server-chosen
                    // reduced-resolution stream never triggers one. A
                    // background session is resized when it is shown.
                    if is_active(&ui, &s) {
                        request_resize(&ui);
                        if headless && s.cursor.borrow().is_none() {
                            let cursor = hidden_cursor();
                            ui.video.set_cursor(Some(&cursor));
                            *s.cursor.borrow_mut() = Some(cursor);
                        }
                    }
                }
                Status::Stats {
                    fps,
                    mbit,
                    decode_ms,
                    video,
                } => {
                    let (sw, sh) = s.stream_size.get();
                    let (vw, vh) = s.stream_view.get();
                    let stream = if (sw, sh) == (vw, vh) {
                        format!("{sw}x{sh}")
                    } else {
                        format!("{sw}x{sh} → {vw}x{vh}")
                    };
                    let cap = s.fps_cap.get();
                    set_stats(
                        &ui,
                        &s,
                        &format!(
                            "{video}  {fps:.0} fps  {mbit:.1} Mbit/s  decode {decode_ms:.1} ms  {stream} @{cap}"
                        ),
                    );
                }
                Status::Cursor(latest) => {
                    if let Some(shape) = latest.take() {
                        set_remote_cursor(
                            &ui,
                            &s,
                            CursorImage {
                                width: shape.width,
                                height: shape.height,
                                hot_x: shape.hot_x,
                                hot_y: shape.hot_y,
                                argb: &shape.argb,
                            },
                        )
                    }
                }
                // Only the shown machine may take over the local clipboard.
                Status::ClipboardOffer {
                    serial,
                    mime_types,
                    files,
                } => {
                    if !ui.cli.no_clipboard && is_active(&ui, &s) {
                        if let Some(tx) = s.input_tx.borrow().as_ref() {
                            clipboard_ui::set_remote_offer(tx, serial, mime_types, files);
                        }
                    }
                }
                Status::ClipboardRead {
                    mime_type,
                    generation,
                    reply,
                } => {
                    let shown_by = ui.clone();
                    serve_local_read(
                        &s,
                        ui.cli.no_clipboard,
                        mime_type,
                        generation,
                        reply,
                        move || (active(&shown_by), shown_by.local_clipboard.borrow().clone()),
                    );
                }
                Status::ClipboardTransfer { id, progress } => {
                    let conn = s.conn.get();
                    let cancel_via = s.clone();
                    ui.transfers.update(conn, id, &progress, move |id| {
                        // After a reconnect the id belongs to another job.
                        if cancel_via.conn.get() != conn {
                            return;
                        }
                        if let Some(tx) = cancel_via.input_tx.borrow().clone() {
                            let _ = tx.send(clipboard::ToWorker::CancelTransfer(id));
                        }
                    });
                }
                Status::Error(e) => {
                    tracing::error!(machine = %s.name, error = %e, "connection failed");
                    show_disconnected(&ui, &s);
                    schedule_reconnect(ui.clone(), s.clone(), generation, Some(&e));
                    return glib::ControlFlow::Break;
                }
                Status::Incompatible(message) => {
                    *s.last_error.borrow_mut() = Some(message.clone());
                    tracing::error!(machine = %s.name, error = %message, "incompatible server");
                    set_status(&ui, &s, &message);
                    show_disconnected(&ui, &s);
                    s.gave_up.set(true);
                    return glib::ControlFlow::Break;
                }
                Status::Closed => {
                    show_disconnected(&ui, &s);
                    schedule_reconnect(ui.clone(), s.clone(), generation, None);
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Show the remote cursor as the video widget's own cursor, so the local
/// compositor draws it at the real pointer position with no added latency.
/// An image with no visible shape (fully transparent, or one flat colour as
/// Hyprland sends when it has no cursor image to share) falls back to the
/// default pointer — except on a headless session, whose frames already
/// carry the compositor-drawn cursor and would show two pointers.
fn set_remote_cursor(ui: &App, s: &Rc<Session>, image: CursorImage) {
    let cursor = remote_cursor(image).or_else(|| s.headless.get().then(hidden_cursor));
    if is_active(ui, s) {
        ui.video.set_cursor(cursor.as_ref());
    }
    *s.cursor.borrow_mut() = cursor;
}

/// A fully transparent 1×1 cursor: the compositor shows nothing for the
/// local pointer while the video carries the remote one.
fn hidden_cursor() -> gdk::Cursor {
    let bytes = glib::Bytes::from_owned(vec![0u8; 4]);
    let texture = gdk::MemoryTexture::new(1, 1, gdk::MemoryFormat::B8g8r8a8, &bytes, 4);
    gdk::Cursor::from_texture(&texture, 0, 0, None)
}

struct CursorImage<'a> {
    width: u32,
    height: u32,
    hot_x: i32,
    hot_y: i32,
    argb: &'a [u8],
}

fn remote_cursor(image: CursorImage) -> Option<gdk::Cursor> {
    let CursorImage {
        width,
        height,
        hot_x,
        hot_y,
        argb,
    } = image;
    if width == 0 || height == 0 || width > 1024 || height > 1024 {
        return None;
    }
    // GTK refuses a hotspot outside the image and returns no cursor, which
    // the bindings would wrap as a valid one.
    if !(0..width as i32).contains(&hot_x) || !(0..height as i32).contains(&hot_y) {
        return None;
    }
    let needed = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))?;
    if argb.len() < needed {
        return None;
    }
    if !has_visible_shape(argb) {
        return None;
    }
    let bytes = glib::Bytes::from(argb);
    let texture = gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::B8g8r8a8,
        &bytes,
        width as usize * 4,
    );
    Some(gdk::Cursor::from_texture(&texture, hot_x, hot_y, None))
}

fn has_visible_shape(argb: &[u8]) -> bool {
    let mut pixels = argb.chunks_exact(4);
    let Some(first) = pixels.next() else {
        return false;
    };
    let opaque = first[3] != 0 || pixels.clone().any(|p| p[3] != 0);
    let uniform = pixels.all(|p| p == first);
    opaque && !uniform
}

/// Map a widget-space point to the remote output's logical coordinates:
/// invert the frame layout to view pixels, scale to physical stream pixels
/// (a reduced-resolution stream is stretched to the view box), then divide
/// by the remote scale, which is what the virtual pointer expects.
fn to_remote(ui: &App, x: f64, y: f64) -> (f64, f64) {
    let Some(s) = active(ui) else {
        return (0.0, 0.0);
    };
    let (rw, rh) = s.stream_size.get();
    let (vw, vh) = s.stream_view.get();
    if rw == 0 || rh == 0 {
        return (0.0, 0.0);
    }
    let device = device_scale(ui);
    let (aw, ah) = (ui.video.width() as f64, ui.video.height() as f64);
    let Some(l) = paintable::layout((vw, vh), device, aw, ah) else {
        return (0.0, 0.0);
    };
    let vx = (x - l.x) / l.factor * device;
    let vy = (y - l.y) / l.factor * device;
    let px = (vx * rw as f64 / vw as f64).clamp(0.0, rw as f64);
    let py = (vy * rh as f64 / vh as f64).clamp(0.0, rh as f64);
    let scale = s.stream_scale.get().max(0.01) as f64;
    (px / scale, py / scale)
}

fn send(ui: &App, msg: ClientMsg) {
    if let Some(s) = active(ui) {
        send_to(&s, msg);
    }
}

fn send_to(s: &Session, msg: ClientMsg) {
    if let Some(tx) = s.input_tx.borrow().as_ref() {
        let _ = tx.send(clipboard::ToWorker::Send(msg));
    }
}

/// How the user releases captured shortcuts back to the local compositor.
#[derive(Clone)]
enum ReleaseHotkey {
    None,
    DoubleTap {
        keyval: gdk::Key,
        within: Duration,
    },
    Chord {
        mods: gdk::ModifierType,
        keyval: gdk::Key,
    },
}

const CHORD_MODS: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK
    .union(gdk::ModifierType::ALT_MASK)
    .union(gdk::ModifierType::SHIFT_MASK)
    .union(gdk::ModifierType::SUPER_MASK);

impl ReleaseHotkey {
    fn parse(spec: &str) -> Result<Self, String> {
        let s = spec.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("none") {
            return Ok(Self::None);
        }
        let lower = s.to_ascii_lowercase();
        if let Some(rest) = lower
            .strip_prefix("double-")
            .or_else(|| lower.strip_prefix("double:"))
        {
            return Ok(Self::DoubleTap {
                keyval: key_from_name(rest)?,
                within: Duration::from_millis(400),
            });
        }
        let mut mods = gdk::ModifierType::empty();
        let mut keyval = None;
        for tok in s.split('+') {
            match tok.trim().to_ascii_lowercase().as_str() {
                "" => {}
                "ctrl" | "control" => mods |= gdk::ModifierType::CONTROL_MASK,
                "alt" => mods |= gdk::ModifierType::ALT_MASK,
                "shift" => mods |= gdk::ModifierType::SHIFT_MASK,
                "super" | "logo" | "win" | "meta" => mods |= gdk::ModifierType::SUPER_MASK,
                other => keyval = Some(key_from_name(other)?),
            }
        }
        match keyval {
            Some(keyval) => Ok(Self::Chord { mods, keyval }),
            None => Err(format!("no key in release hotkey '{spec}'")),
        }
    }

    /// True if this press is the release trigger. For a double-tap it records
    /// the tap time and returns true only on the quick second press, so the
    /// first tap still reaches the remote.
    fn matches(
        &self,
        keyval: gdk::Key,
        state: gdk::ModifierType,
        unshifted: Option<gdk::Key>,
        last_tap: &RefCell<Option<Instant>>,
    ) -> bool {
        match self {
            Self::None => false,
            Self::Chord { mods, keyval: k } => {
                (same_key(keyval, *k) || unshifted.is_some_and(|base| same_key(base, *k)))
                    && (state & CHORD_MODS) == *mods
            }
            Self::DoubleTap { keyval: k, within } => {
                if !same_key(keyval, *k) && !unshifted.is_some_and(|base| same_key(base, *k)) {
                    return false;
                }
                let now = Instant::now();
                let mut lt = last_tap.borrow_mut();
                match *lt {
                    Some(prev) if now.duration_since(prev) <= *within => {
                        *lt = None;
                        true
                    }
                    _ => {
                        *lt = Some(now);
                        false
                    }
                }
            }
        }
    }

    fn describe(&self) -> String {
        let name = |k: &gdk::Key| {
            let n = k
                .name()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".into());
            if n == "Escape" {
                "Esc".into()
            } else {
                n
            }
        };
        match self {
            Self::None => "release disabled".into(),
            Self::DoubleTap { keyval, .. } => format!("double-tap {}", name(keyval)),
            Self::Chord { mods, keyval } => {
                let mut parts = Vec::new();
                if mods.contains(gdk::ModifierType::CONTROL_MASK) {
                    parts.push("Ctrl".to_string());
                }
                if mods.contains(gdk::ModifierType::ALT_MASK) {
                    parts.push("Alt".to_string());
                }
                if mods.contains(gdk::ModifierType::SHIFT_MASK) {
                    parts.push("Shift".to_string());
                }
                if mods.contains(gdk::ModifierType::SUPER_MASK) {
                    parts.push("Super".to_string());
                }
                parts.push(name(keyval));
                parts.join("+")
            }
        }
    }
}

/// True if a pressed keyval is the hotkey's key. GTK reports the keyval with
/// Shift and Caps Lock applied, so Ctrl+Shift+q arrives as `Q` and Shift+Tab
/// as `ISO_Left_Tab`; normalize those direct keyvals. The event handler also
/// translates the hardware key without Shift/Lock for punctuation keys.
fn same_key(pressed: gdk::Key, key: gdk::Key) -> bool {
    let unshifted = |k: gdk::Key| {
        if k == gdk::Key::ISO_Left_Tab {
            gdk::Key::Tab
        } else {
            k.to_lower()
        }
    };
    unshifted(pressed) == unshifted(key)
}

/// Resolve a key name to a `gdk::Key`, accepting lower-case and a few aliases.
fn key_from_name(name: &str) -> Result<gdk::Key, String> {
    let n = name.trim();
    let alias = match n.to_ascii_lowercase().as_str() {
        "esc" => Some("Escape"),
        "enter" | "return" => Some("Return"),
        "space" => Some("space"),
        _ => None,
    };
    let title = {
        let mut c = n.chars();
        c.next()
            .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
            .unwrap_or_default()
    };
    for cand in alias.iter().copied().chain([n, title.as_str()]) {
        if !cand.is_empty() {
            if let Some(k) = gdk::Key::from_name(cand) {
                return Ok(k);
            }
        }
    }
    Err(format!("unknown key '{name}'"))
}

/// Give the keyboard back to the local compositor by dropping video focus,
/// which fires the focus-leave handler that restores system shortcuts.
/// True while the address bar shows and holds the focus. GTK4 focuses the
/// text inside the entry, so ask the window for the focus widget and walk
/// up; a folded address bar is unmapped.
fn address_bar_in_use(window: &adw::ApplicationWindow, entry: &gtk::Entry) -> bool {
    entry.is_mapped()
        && gtk::prelude::GtkWindowExt::focus(window)
            .is_some_and(|f| f == *entry || f.is_ancestor(entry))
}

fn release_capture(ui: &Rc<App>, window: &adw::ApplicationWindow) {
    ui.released.set(true);
    gtk::prelude::GtkWindowExt::set_focus(window, gtk::Widget::NONE);
    show_hint(ui, "Shortcuts released — click the screen to capture again");
}

/// Release every key held on the remote. Once the video loses focus their
/// local release events never reach us, so the remote would keep (say) Shift
/// down until the next connection.
fn release_pressed_keys(ui: &App) {
    let held = std::mem::take(&mut *ui.pressed_keys.borrow_mut());
    for code in held {
        send(
            ui,
            ClientMsg::Key {
                keycode: code,
                pressed: false,
            },
        );
    }
}

/// Record a local key event. Returns false when it should not be forwarded:
/// an auto-repeat press (the remote compositor repeats on its own) or a
/// release of a key whose press was never sent.
fn track_key(ui: &App, code: u32, pressed: bool) -> bool {
    let mut keys = ui.pressed_keys.borrow_mut();
    if pressed {
        keys.insert(code)
    } else {
        keys.remove(&code)
    }
}

// GTK surface deltas are already logical pixels, not wheel detents.
fn scroll_delta(delta: f64, surface: bool) -> (f64, Option<i32>) {
    if surface {
        (delta, None)
    } else {
        (delta * 15.0, Some(delta.signum() as i32))
    }
}

fn install_input_handlers(
    ui: &Rc<App>,
    video: &gtk::Widget,
    window: &adw::ApplicationWindow,
    hotkey: ReleaseHotkey,
) {
    video.set_focusable(true);
    video.set_can_focus(true);

    // Keyboard: hardware keycode minus 8 is the evdev code. The controller
    // runs in the capture phase so that while the picture has the keyboard
    // every key goes to the remote, ahead of any local handling.
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let last_tap: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));
    {
        let ui = ui.clone();
        let window = window.clone();
        let hotkey = hotkey.clone();
        let last_tap = last_tap.clone();
        key.connect_key_pressed(move |controller, keyval, keycode, state| {
            let group = controller
                .current_event()
                .and_then(|event| {
                    event
                        .downcast_ref::<gdk::KeyEvent>()
                        .map(|key| key.layout())
                })
                .unwrap_or(0) as i32;
            let base_state =
                state & !(gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::LOCK_MASK);
            let unshifted = gtk::prelude::WidgetExt::display(&window)
                .translate_key(keycode, base_state, group)
                .map(|(base, _, _, _)| base);
            if hotkey.matches(keyval, state, unshifted, &last_tap) {
                release_pressed_keys(&ui);
                release_capture(&ui, &window);
                return glib::Propagation::Stop;
            }
            let code = keycode.saturating_sub(8);
            tracing::debug!(code, "key pressed");
            if track_key(&ui, code, true) {
                send(
                    &ui,
                    ClientMsg::Key {
                        keycode: code,
                        pressed: true,
                    },
                );
            }
            glib::Propagation::Stop
        });
    }
    {
        let ui = ui.clone();
        key.connect_key_released(move |_, _keyval, keycode, _state| {
            let code = keycode.saturating_sub(8);
            tracing::debug!(code, "key released");
            if track_key(&ui, code, false) {
                send(
                    &ui,
                    ClientMsg::Key {
                        keycode: code,
                        pressed: false,
                    },
                );
            }
        });
    }
    video.add_controller(key);

    // Pointer motion, and the capture itself: the remote gets the keyboard
    // only while the pointer is over the picture, so the header bar and
    // anything else outside it stay local.
    let motion = gtk::EventControllerMotion::new();
    {
        let ui = ui.clone();
        motion.connect_motion(move |_, x, y| {
            let (rx, ry) = to_remote(&ui, x, y);
            send(&ui, ClientMsg::PointerMotion { x: rx, y: ry });
        });
    }
    {
        let ui = ui.clone();
        let video = video.clone();
        motion.connect_enter(move |_, _, _| {
            if active(&ui).is_some()
                && !ui.released.get()
                && !address_bar_in_use(&ui.window, &ui.entry)
            {
                video.grab_focus();
            }
        });
    }
    {
        let ui = ui.clone();
        let window = window.clone();
        motion.connect_leave(move |_| {
            // A deliberate release lasts only as long as the pointer rests on
            // the picture; leaving and coming back captures again.
            ui.released.set(false);
            if gtk::prelude::GtkWindowExt::focus(&window).is_some_and(|f| f == ui.video) {
                gtk::prelude::GtkWindowExt::set_focus(&window, gtk::Widget::NONE);
            }
        });
    }
    video.add_controller(motion);

    // Buttons, each press and release as it comes. A click gesture follows
    // one button at a time and cancels it when a second button is pressed,
    // or, for back, forward and other buttons past the fifth, when the
    // pointer moves; the release then never reaches the remote, which keeps
    // the button down.
    let buttons = gtk::EventControllerLegacy::new();
    {
        let ui = ui.clone();
        let video = video.clone();
        buttons.connect_event(move |_, event| {
            let kind = event.event_type();
            let button = match kind {
                gdk::EventType::ButtonPress | gdk::EventType::ButtonRelease => event
                    .downcast_ref::<gdk::ButtonEvent>()
                    .map(|e| evdev_button(e.button())),
                // The first finger on a touch screen is the left button.
                gdk::EventType::TouchBegin
                | gdk::EventType::TouchEnd
                | gdk::EventType::TouchCancel => event
                    .downcast_ref::<gdk::TouchEvent>()
                    .filter(|e| e.is_emulating_pointer())
                    .map(|_| evdev_button(1)),
                _ => None,
            };
            let Some(button) = button else {
                return glib::Propagation::Proceed;
            };
            let pressed = matches!(
                kind,
                gdk::EventType::ButtonPress | gdk::EventType::TouchBegin
            );
            if pressed {
                ui.released.set(false);
                video.grab_focus();
            }
            send(&ui, ClientMsg::PointerButton { button, pressed });
            glib::Propagation::Proceed
        });
    }
    video.add_controller(buttons);

    // Scroll.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    {
        let ui = ui.clone();
        scroll.connect_scroll(move |controller, dx, dy| {
            let surface = controller.unit() == gdk::ScrollUnit::Surface;
            if dy != 0.0 {
                send(
                    &ui,
                    ClientMsg::PointerAxis {
                        axis: Axis::Vertical,
                        value: scroll_delta(dy, surface).0,
                        discrete: scroll_delta(dy, surface).1,
                        stop: false,
                    },
                );
            }
            if dx != 0.0 {
                send(
                    &ui,
                    ClientMsg::PointerAxis {
                        axis: Axis::Horizontal,
                        value: scroll_delta(dx, surface).0,
                        discrete: scroll_delta(dx, surface).1,
                        stop: false,
                    },
                );
            }
            glib::Propagation::Stop
        });
    }
    {
        let ui = ui.clone();
        scroll.connect_scroll_end(move |_| {
            for axis in [Axis::Vertical, Axis::Horizontal] {
                send(
                    &ui,
                    ClientMsg::PointerAxis {
                        axis,
                        value: 0.0,
                        discrete: None,
                        stop: true,
                    },
                );
            }
        });
    }
    video.add_controller(scroll);

    // While the video has focus, route system shortcuts (Super, Alt-Tab, ...)
    // to the remote session instead of the local compositor.
    // The compositor answers asynchronously, and may refuse.
    window.connect_realize(|window| {
        if let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() {
            toplevel.connect_shortcuts_inhibited_notify(|t| {
                tracing::info!(
                    inhibited = t.is_shortcuts_inhibited(),
                    "compositor shortcut inhibit"
                );
            });
        }
    });
    let focus = gtk::EventControllerFocus::new();
    {
        let window = window.clone();
        let ui = ui.clone();
        let hint = match &hotkey {
            ReleaseHotkey::None => None,
            hk => Some(format!("Shortcuts captured — {} to release", hk.describe())),
        };
        focus.connect_enter(move |_| {
            tracing::debug!("video focused; inhibiting system shortcuts");
            if let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() {
                toplevel.inhibit_system_shortcuts(None::<&gdk::Event>);
            }
            if let Some(hint) = &hint {
                if !ui.capture_hinted.replace(true) {
                    show_hint(&ui, hint);
                }
            }
        });
    }
    {
        let window = window.clone();
        let ui = ui.clone();
        focus.connect_leave(move |_| {
            tracing::debug!("video unfocused; restoring system shortcuts");
            release_pressed_keys(&ui);
            if let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() {
                toplevel.restore_system_shortcuts();
            }
        });
    }
    video.add_controller(focus);
}

/// In fullscreen the header leaves the layout, so the picture gets the whole
/// screen, and slides in over it while the pointer is at the top edge. The header stays while the address bar is in use, so
/// it does not slide away under a half-typed machine.
fn install_fullscreen_bars(
    window: &adw::ApplicationWindow,
    content: &gtk::Box,
    overlay: &gtk::Overlay,
    header: &adw::HeaderBar,
    fullscreen_btn: &gtk::ToggleButton,
    entry: &gtk::Entry,
) {
    let address_bar_in_use = {
        let (window, entry) = (window.clone(), entry.clone());
        Rc::new(move || address_bar_in_use(&window, &entry))
    };
    let top = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .valign(gtk::Align::Start)
        .build();
    overlay.add_overlay(&top);

    {
        let (content, header) = (content.clone(), header.clone());
        let (top, fullscreen_btn) = (top.clone(), fullscreen_btn.clone());
        window.connect_fullscreened_notify(move |w| {
            let full = w.is_fullscreen();
            if fullscreen_btn.is_active() != full {
                fullscreen_btn.set_active(full);
            }
            if full {
                content.remove(&header);
                top.set_child(Some(&header));
            } else {
                top.set_reveal_child(false);
                top.set_child(None::<&gtk::Widget>);
                content.prepend(&header);
            }
        });
    }

    const EDGE: f64 = 2.0;
    const LEAVE_MARGIN: f64 = 8.0;
    let motion = gtk::EventControllerMotion::new();
    {
        let (window, header, top) = (window.clone(), header.clone(), top.clone());
        let address_bar_in_use = address_bar_in_use.clone();
        motion.connect_motion(move |_, _, y| {
            if !window.is_fullscreen() {
                return;
            }
            if y <= EDGE {
                top.set_reveal_child(true);
            } else if top.reveals_child()
                && y > header.height() as f64 + LEAVE_MARGIN
                && !address_bar_in_use()
            {
                top.set_reveal_child(false);
            }
        });
    }
    {
        let top = top.clone();
        let address_bar_in_use = address_bar_in_use.clone();
        motion.connect_leave(move |_| {
            if !address_bar_in_use() {
                top.set_reveal_child(false);
            }
        });
    }
    overlay.add_controller(motion);

    // Keyboard dismissal (Escape, Enter) moves focus without a pointer
    // event; hide the header then, unless the pointer still rests on it.
    {
        let (window, top, header) = (window.clone(), top.clone(), header.clone());
        window.clone().connect_focus_widget_notify(move |_| {
            if !window.is_fullscreen() || address_bar_in_use() {
                return;
            }
            let pointer_on_header = WidgetExt::display(&window)
                .default_seat()
                .and_then(|s| s.pointer())
                .and_then(|p| window.surface().and_then(|s| s.device_position(&p)))
                .is_some_and(|(_, y, _)| y <= header.height() as f64 + LEAVE_MARGIN);
            if !pointer_on_header {
                top.set_reveal_child(false);
            }
        });
    }
}

/// Ask the server to match the window, once the size has settled for 200 ms
/// so a drag-resize does not restart the encoder on every step. The
/// decoder's output zoom is requested on each poll that changes it, with
/// no settling delay: on the GPU tier the change costs only a reallocation
/// of the display images and a redraw, and the CPU tier picks it up with
/// its next frame.
fn install_resize_handler(ui: &Rc<App>) {
    // A Picture has no resize signal, so poll its allocation; the one-shot
    // timer sends only once the size has held for 200 ms.
    let ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let scale = device_scale(&ui);
        ui.frame.set_scale(scale);
        request_zoom(&ui);
        let (w, h) = (
            ui.video.width() as f64 * scale,
            ui.video.height() as f64 * scale,
        );
        let size = (
            w.max(0.0).round() as u32 & !1,
            h.max(0.0).round() as u32 & !1,
        );
        let scale_changed = ui.view_scale.replace(scale) != scale;
        if size != ui.view_size.get() || scale_changed {
            ui.view_size.set(size);
            let ui = ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(200), move || {
                if ui.view_size.get() == size {
                    request_resize(&ui);
                }
            });
        }
        glib::ControlFlow::Continue
    });
}

/// Ask the decoder for the integer zoom the current window allows, when it
/// differs from the last request. The view comes from the latest
/// StreamConfig, falling back to the last painted frame.
fn request_zoom(ui: &App) {
    let Some(s) = active(ui) else {
        return;
    };
    let mut view = s.server_view.get();
    if view == (0, 0) {
        view = s.stream_view.get();
    }
    let device = device_scale(ui);
    let (aw, ah) = (ui.video.width() as f64, ui.video.height() as f64);
    let want = paintable::layout(view, device, aw, ah).map_or(1, |l| l.zoom());
    if want == s.zoom.get() {
        return;
    }
    s.zoom.set(want);
    let tx = s.input_tx.borrow().clone();
    if let Some(tx) = tx {
        let _ = tx.send(clipboard::ToWorker::Zoom(want));
    }
}

/// Send a Resize if the server's view does not already match the window.
/// The gate compares the server's VIEW, not the stream: a stream the server
/// chose to send at reduced resolution is not a size mismatch, and asking
/// again would fight the server's own choice.
fn request_resize(ui: &App) {
    let Some(s) = active(ui) else {
        return;
    };
    let size = ui.view_size.get();
    let scale = device_scale(ui);
    if size.0 < 64
        || size.1 < 64
        || (s.resize_scale.get() == scale
            && (size == s.server_view.get() || size == s.resize_requested.get()))
    {
        return;
    }
    s.resize_requested.set(size);
    s.resize_scale.set(scale);
    send_to(
        &s,
        ClientMsg::Resize {
            width: size.0,
            height: size.1,
            scale: scale as f32,
        },
    );
}

/// GTK button number to evdev `BTN_*`. GDK numbers left, middle and right
/// 1-3 and every further button `BTN_*` - `BTN_LEFT` + 5, after the old
/// scroll buttons 4-7: 8 is `BTN_SIDE` (back), 9 is `BTN_EXTRA` (forward).
fn evdev_button(n: u32) -> u32 {
    match n {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        8.. => 0x110 + n - 5,
        _ => 0x110,
    }
}

fn device_scale(ui: &App) -> f64 {
    ui.window
        .surface()
        .map_or(ui.video.scale_factor() as f64, |s| s.scale())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    type Input = tokio::sync::mpsc::UnboundedReceiver<clipboard::ToWorker>;

    fn session_with_input() -> (Rc<Session>, Input) {
        let s = Rc::new(Session::new(Endpoint::Tcp("127.0.0.1:9070".into())));
        let (tx, rx) = unbounded_channel();
        *s.input_tx.borrow_mut() = Some(tx);
        (s, rx)
    }

    /// What a session's worker was sent about the local clipboard.
    #[derive(Debug, PartialEq)]
    enum Queued {
        Revoke,
        /// An offer's mime types and generation.
        Offer(Vec<String>, u64),
    }

    fn queued(rx: &mut Input) -> Vec<Queued> {
        let mut all = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            all.push(match msg {
                clipboard::ToWorker::RevokeLocal => Queued::Revoke,
                clipboard::ToWorker::LocalOffer {
                    mime_types,
                    files,
                    generation,
                } => {
                    assert!(files.entries.is_empty());
                    Queued::Offer(mime_types, generation)
                }
                _ => panic!("expected a clipboard message"),
            });
        }
        all
    }

    fn text(generation: u64) -> Queued {
        Queued::Offer(vec!["text/plain".into()], generation)
    }

    /// What a switch sends the session shown before it.
    fn withdrawn() -> Vec<Queued> {
        vec![Queued::Revoke, Queued::Offer(Vec::new(), 0)]
    }

    /// A tab bar: the sessions, the shown one, and the offer routing the
    /// clipboard watcher in `build_ui` uses.
    struct Tabs {
        shown: Rc<RefCell<Option<Rc<Session>>>>,
        watch: clipboard_ui::LocalSelection,
        no_clipboard: bool,
    }

    impl Tabs {
        fn new(sessions: Vec<Rc<Session>>, no_clipboard: bool) -> Self {
            let sessions = Rc::new(sessions);
            let shown = Rc::new(RefCell::new(None::<Rc<Session>>));
            let watch = {
                let running = sessions.clone();
                let sessions = sessions.clone();
                let shown = shown.clone();
                clipboard_ui::LocalSelection::new(
                    move || running_inputs(&running, no_clipboard),
                    move || local_offer_targets(&sessions, shown.borrow().as_ref(), no_clipboard),
                )
            };
            Self {
                shown,
                watch,
                no_clipboard,
            }
        }

        /// What `show_session` does on a switch.
        fn show(&self, s: &Rc<Session>) {
            let previous = self.shown.replace(Some(s.clone()));
            move_local_offer(previous.as_ref(), self.no_clipboard, Some(&self.watch));
        }

        /// A local copy whose listing has started.
        fn start_copy(&self) -> u64 {
            self.watch.changed(false).expect("a local copy is offered")
        }

        /// A new local copy whose listing completes at once. Without
        /// clipboard sharing no session is revoked and nothing is listed.
        fn copy(&self, mime: &str) -> u64 {
            let Some(gen) = self.watch.changed(false) else {
                return self.watch.generation();
            };
            self.watch
                .finish(gen, vec![mime.into()], Default::default());
            gen
        }

        /// Queue a server's read for `s` the way the status handler does.
        fn queue_read(
            &self,
            s: &Rc<Session>,
            generation: u64,
        ) -> tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>> {
            let (reply, rx) = tokio::sync::mpsc::channel(2);
            let shown = self.shown.clone();
            let watch = self.watch.clone();
            serve_local_read(
                s,
                self.no_clipboard,
                "text/plain".into(),
                generation,
                reply,
                move || (shown.borrow().clone(), Some(watch.clone())),
            );
            rx
        }

        fn may_read(&self, s: &Rc<Session>, generation: u64) -> bool {
            may_read_local(
                self.shown.borrow().as_ref(),
                s,
                self.no_clipboard,
                generation,
                Some(&self.watch),
            )
        }
    }

    #[test]
    fn only_the_shown_session_is_offered_the_local_clipboard() {
        let (a, mut a_rx) = session_with_input();
        let (b, mut b_rx) = session_with_input();
        let tabs = Tabs::new(vec![a.clone(), b.clone()], false);
        tabs.copy("text/plain");
        assert_eq!(queued(&mut a_rx), [Queued::Revoke]);
        assert_eq!(queued(&mut b_rx), [Queued::Revoke]);

        tabs.shown.replace(Some(b.clone()));
        let gen = tabs.copy("text/plain");
        assert_eq!(queued(&mut a_rx), [Queued::Revoke]);
        assert_eq!(queued(&mut b_rx), [Queued::Revoke, text(gen)]);

        let tabs = Tabs::new(vec![a.clone(), b.clone()], true);
        tabs.shown.replace(Some(b.clone()));
        tabs.copy("text/plain");
        assert_eq!(queued(&mut a_rx), []);
        assert_eq!(queued(&mut b_rx), []);
    }

    #[test]
    fn the_servers_offer_taking_the_clipboard_revokes_every_session() {
        let (a, mut a_rx) = session_with_input();
        let (b, mut b_rx) = session_with_input();
        let tabs = Tabs::new(vec![a.clone(), b.clone()], false);
        tabs.show(&b);
        tabs.copy("text/plain");
        queued(&mut a_rx);
        queued(&mut b_rx);
        assert_eq!(tabs.watch.changed(true), None);
        assert_eq!(queued(&mut a_rx), [Queued::Revoke]);
        assert_eq!(queued(&mut b_rx), [Queued::Revoke]);
    }

    #[test]
    fn switching_tabs_moves_the_local_offer() {
        let (a, mut a_rx) = session_with_input();
        let (b, mut b_rx) = session_with_input();
        let tabs = Tabs::new(vec![a.clone(), b.clone()], false);
        tabs.show(&a);
        let first = tabs.copy("text/plain");
        assert_eq!(queued(&mut a_rx), [Queued::Revoke, text(first)]);
        assert_eq!(queued(&mut b_rx), [Queued::Revoke]);

        // A copy of files is still being listed when the tab switches.
        let listing = tabs.start_copy();
        tabs.show(&b);
        tabs.watch
            .finish(listing, vec!["text/uri-list".into()], Default::default());
        let reoffer = tabs.watch.generation();
        tabs.watch
            .finish(reoffer, vec!["text/plain".into()], Default::default());

        let mut a_expected = vec![Queued::Revoke];
        a_expected.extend(withdrawn());
        assert_eq!(queued(&mut a_rx), a_expected);
        assert_eq!(queued(&mut b_rx), [Queued::Revoke, text(reoffer)]);

        // Showing no session withdraws the offer too.
        revoke_local_offer(tabs.shown.take().as_ref(), false);
        assert_eq!(queued(&mut b_rx), withdrawn());
    }

    /// Documents the order a switch queues: the withdrawal, then the new
    /// offer. It does not guard it: the new offer is always sent from a
    /// task after the clipboard is read, whatever order the calls are in.
    #[test]
    fn the_withdrawal_is_queued_before_the_new_offer() {
        // Both tabs share one input, so the order of the two is visible.
        let (a, mut rx) = session_with_input();
        let b = Rc::new(Session::new(Endpoint::Tcp("127.0.0.1:9071".into())));
        *b.input_tx.borrow_mut() = a.input_tx.borrow().clone();
        let tabs = Tabs::new(vec![a.clone(), b.clone()], false);
        tabs.show(&a);
        tabs.show(&b);
        let reoffer = tabs.watch.generation();
        tabs.watch
            .finish(reoffer, vec!["text/plain".into()], Default::default());
        let mut expected = withdrawn();
        expected.push(text(reoffer));
        assert_eq!(queued(&mut rx), expected);
    }

    /// Run the queued reads until `rx` answers: true for a refusal, false
    /// when the read went past the check (it then finds no clipboard without
    /// a display and ends with no bytes).
    fn run_read(
        context: &glib::MainContext,
        rx: &mut tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>>,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            context.iteration(false);
            match rx.try_recv() {
                Ok(Err(_)) => {
                    assert!(rx.try_recv().is_err(), "bytes after a refusal");
                    return true;
                }
                Ok(Ok(_)) => panic!("clipboard bytes were sent"),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return false,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "read did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn the_status_handler_refuses_a_read_outdated_before_it_starts() {
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let (a, _a_rx) = session_with_input();
        let (b, _b_rx) = session_with_input();
        let tabs = Tabs::new(vec![a.clone(), b.clone()], false);
        tabs.show(&a);
        let offered = tabs.copy("text/plain");

        // a's read is queued, then the tab switches before it runs.
        let mut read = tabs.queue_read(&a, offered);
        tabs.show(&b);
        assert!(run_read(&context, &mut read));

        // b's read is queued, then a new copy replaces the selection.
        let current = tabs.watch.generation();
        let mut read = tabs.queue_read(&b, current);
        tabs.copy("image/png");
        assert!(run_read(&context, &mut read));

        // A read for the shown session's current offer is let through.
        let mut read = tabs.queue_read(&b, tabs.watch.generation());
        assert!(!run_read(&context, &mut read));
    }

    #[test]
    fn a_read_queued_before_a_switch_or_a_copy_is_refused() {
        let (a, _a_rx) = session_with_input();
        let (b, _b_rx) = session_with_input();
        let tabs = Tabs::new(vec![a.clone(), b.clone()], false);
        tabs.show(&a);
        let offered = tabs.copy("text/plain");
        assert!(tabs.may_read(&a, offered));
        assert!(!tabs.may_read(&b, offered));

        // A request a's worker accepted before the switch reaches the UI after it.
        tabs.show(&b);
        assert!(!tabs.may_read(&a, offered));

        // b's own request against its offer, then one that a new copy outdated.
        let current = tabs.watch.generation();
        assert!(tabs.may_read(&b, current));
        tabs.copy("image/png");
        assert!(!tabs.may_read(&b, current));

        let tabs = Tabs::new(vec![a.clone()], true);
        tabs.shown.replace(Some(a.clone()));
        assert!(!tabs.may_read(&a, tabs.watch.generation()));
    }

    #[test]
    fn reconnect_invalidates_the_previous_scale_request() {
        let s = Session::new(Endpoint::Tcp("127.0.0.1:9070".into()));
        s.server_view.set((1920, 1080));
        s.resize_requested.set((1920, 1080));
        s.resize_scale.set(1.25);
        s.reset_resize();
        assert_ne!(s.resize_scale.get(), 1.25);
        assert_eq!(s.resize_requested.get(), (0, 0));
    }

    #[test]
    fn reconnect_status_keeps_the_error() {
        let e = "Connection refused (os error 111)";
        assert_eq!(
            connecting_status(Some(e)),
            "Error: Connection refused (os error 111)\nConnecting…"
        );
        assert_eq!(connecting_status(None), "Connecting…");
        assert_eq!(
            reconnect_status(Some(e), 1),
            "Error: Connection refused (os error 111)\nReconnecting… (attempt 1)"
        );
        assert_eq!(
            reconnect_status(Some(e), MAX_RETRIES + 1),
            "Error: Connection refused (os error 111)\nDisconnected — click the tab to reconnect"
        );
        assert_eq!(reconnect_status(None, 2), "Reconnecting… (attempt 2)");
    }

    #[test]
    fn visible_shape_needs_alpha_and_contrast() {
        let transparent = [0u8; 16];
        let black = [0, 0, 0, 255].repeat(4);
        let mut arrow = [0u8; 16];
        arrow[3] = 255;
        assert!(!has_visible_shape(&transparent));
        assert!(!has_visible_shape(&black));
        assert!(has_visible_shape(&arrow));
    }

    #[test]
    fn remote_cursor_rejects_a_hotspot_outside_the_image() {
        // A visible 2x2 shape, so only the hotspot decides. These return
        // before any GTK call, which is why no display is needed here.
        let mut argb = [0u8; 16];
        argb[3] = 255;
        for (hot_x, hot_y) in [(2, 0), (0, 2), (-1, 0), (0, -1), (i32::MAX, i32::MIN)] {
            let image = CursorImage {
                width: 2,
                height: 2,
                hot_x,
                hot_y,
                argb: &argb,
            };
            assert!(remote_cursor(image).is_none(), "{hot_x},{hot_y}");
        }
    }

    #[test]
    fn parse_chord_and_double_and_none() {
        assert!(matches!(
            ReleaseHotkey::parse("none").unwrap(),
            ReleaseHotkey::None
        ));
        assert!(matches!(
            ReleaseHotkey::parse("").unwrap(),
            ReleaseHotkey::None
        ));

        match ReleaseHotkey::parse("shift+escape").unwrap() {
            ReleaseHotkey::Chord { mods, keyval } => {
                assert_eq!(mods, gdk::ModifierType::SHIFT_MASK);
                assert_eq!(keyval, gdk::Key::Escape);
            }
            _ => panic!("expected chord"),
        }

        match ReleaseHotkey::parse("Ctrl+Alt+q").unwrap() {
            ReleaseHotkey::Chord { mods, .. } => {
                assert!(mods.contains(gdk::ModifierType::CONTROL_MASK));
                assert!(mods.contains(gdk::ModifierType::ALT_MASK));
            }
            _ => panic!("expected chord"),
        }

        assert!(matches!(
            ReleaseHotkey::parse("double-escape").unwrap(),
            ReleaseHotkey::DoubleTap { .. }
        ));
        assert!(ReleaseHotkey::parse("ctrl+alt").is_err());
    }

    #[test]
    fn chord_matches_only_with_exact_mods() {
        let hk = ReleaseHotkey::parse("shift+escape").unwrap();
        let lt = RefCell::new(None);
        assert!(hk.matches(gdk::Key::Escape, gdk::ModifierType::SHIFT_MASK, None, &lt));
        // Bare Escape, no Shift: not a match (so it reaches the remote).
        assert!(!hk.matches(gdk::Key::Escape, gdk::ModifierType::empty(), None, &lt));
        // Extra lock bits are ignored.
        assert!(hk.matches(
            gdk::Key::Escape,
            gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::LOCK_MASK,
            None,
            &lt
        ));
    }

    #[test]
    fn chord_matches_the_shifted_keyval() {
        let lt = RefCell::new(None);
        let ctrl = gdk::ModifierType::CONTROL_MASK;
        let alt = gdk::ModifierType::ALT_MASK;
        let shift = gdk::ModifierType::SHIFT_MASK;
        // GTK reports Ctrl+Shift+q as Q, and Shift+Tab as ISO_Left_Tab.
        let hk = ReleaseHotkey::parse("ctrl+shift+q").unwrap();
        assert!(hk.matches(gdk::Key::Q, ctrl | shift, None, &lt));
        let hk = ReleaseHotkey::parse("shift+tab").unwrap();
        assert!(hk.matches(gdk::Key::ISO_Left_Tab, shift, None, &lt));
        // Caps Lock turns q into Q as well.
        let hk = ReleaseHotkey::parse("ctrl+alt+q").unwrap();
        let lock = gdk::ModifierType::LOCK_MASK;
        assert!(hk.matches(gdk::Key::Q, ctrl | alt | lock, None, &lt));
        assert!(!hk.matches(gdk::Key::W, ctrl | alt, None, &lt));
    }

    #[test]
    fn shifted_punctuation_matches_its_unshifted_hardware_key() {
        let lt = RefCell::new(None);
        let mods = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
        for (spec, shifted, base) in [
            ("ctrl+shift+1", gdk::Key::exclam, gdk::Key::_1),
            (
                "ctrl+shift+bracketleft",
                gdk::Key::braceleft,
                gdk::Key::bracketleft,
            ),
        ] {
            let hotkey = ReleaseHotkey::parse(spec).unwrap();
            assert!(hotkey.matches(shifted, mods, Some(base), &lt));
            assert!(!hotkey.matches(shifted, mods, None, &lt));
        }
    }

    #[test]
    fn double_tap_needs_two_quick_presses() {
        let hk = ReleaseHotkey::parse("double-escape").unwrap();
        let lt = RefCell::new(None);
        let none = gdk::ModifierType::empty();
        assert!(!hk.matches(gdk::Key::Escape, none, None, &lt)); // first tap forwarded
        assert!(hk.matches(gdk::Key::Escape, none, None, &lt)); // quick second releases
        assert!(!hk.matches(gdk::Key::Escape, none, None, &lt)); // counter reset
    }
}

#[cfg(test)]
mod trackpad_tests {
    use super::scroll_delta;

    #[test]
    fn small_finger_events_preserve_distance_without_wheel_clicks() {
        let events = [0.1, 0.2, 0.3, -0.1];
        let total: f64 = events
            .iter()
            .map(|d| {
                let (value, discrete) = scroll_delta(*d, true);
                assert_eq!(discrete, None);
                value
            })
            .sum();
        assert!((total - 0.5).abs() < 1e-10);
    }

    #[test]
    fn real_wheel_keeps_detents_and_direction() {
        assert_eq!(scroll_delta(1.0, false), (15.0, Some(1)));
        assert_eq!(scroll_delta(-1.0, false), (-15.0, Some(-1)));
    }

    #[test]
    fn gtk_buttons_map_back_to_their_evdev_codes() {
        // GDK on Wayland numbers a button past the first three as its evdev
        // code - BTN_LEFT + 5, so the remote app sees the same number.
        let codes: Vec<u32> = (1..=11).map(super::evdev_button).collect();
        assert_eq!(&codes[..3], [0x110, 0x112, 0x111]);
        assert_eq!(&codes[7..], [0x113, 0x114, 0x115, 0x116]);
    }
}
