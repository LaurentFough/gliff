//! The GTK thread's half of the clipboard: watches the local `gdk::Clipboard`
//! for changes to offer to the server, reads a local item when the server
//! requests one, and proxies the server's offer as a lazy content provider
//! whose bytes are fetched only when an application pastes.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use gtk::gdk;
use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;
use tokio::sync::mpsc::{Sender, UnboundedSender, WeakUnboundedSender};
use tokio::sync::oneshot;

use gliff_proto::clipboard::{
    forwardable_mimes, is_file_mime, local_mimes_for_offer, offers_files, parse_uri_list,
    resolve_mime, CHUNK,
};
use gliff_proto::ClipboardFile;
use gliff_transport::clipboard::files::{list_files, LocalFiles};
use gliff_transport::clipboard::progress::{describe, Progress, State};

use crate::clipboard::ToWorker;

/// Longest `text/uri-list` we read to build an offer.
const URI_LIST_MAX: usize = 1024 * 1024;

fn clipboard() -> Option<gdk::Clipboard> {
    // Unit tests drive the offer routing without a display.
    if !gtk::is_initialized_main_thread() {
        return None;
    }
    gdk::Display::default().map(|d| d.clipboard())
}

fn sensitive_selection(mimes: &[String]) -> bool {
    mimes.iter().any(|m| m == "x-kde-passwordManagerHint")
}

fn holds_remote_offer(cb: &gdk::Clipboard) -> bool {
    cb.content().is_some_and(|p| p.is::<RemoteProvider>())
}

type Senders = Rc<dyn Fn() -> Vec<UnboundedSender<ToWorker>>>;

/// Report every change of the local clipboard to the workers. Each change
/// first revokes the previous local offer in every session `running` names.
/// A change to our own proxy provider stops there, and any other change is
/// then offered to the sessions `shown` names. The returned handle offers
/// the current selection again, for a session that has just been shown.
pub fn watch_local(
    running: impl Fn() -> Vec<UnboundedSender<ToWorker>> + 'static,
    shown: impl Fn() -> Vec<UnboundedSender<ToWorker>> + 'static,
) -> Option<LocalSelection> {
    let cb = clipboard()?;
    let watch = LocalSelection::new(running, shown);
    {
        let watch = watch.clone();
        cb.connect_changed(move |cb| watch.offer(cb));
    }
    Some(watch)
}

/// Offers the local selection to the sessions `shown` names. They are asked
/// when an offer is sent, not when the clipboard changes, so a slow file
/// listing goes to whichever session is shown by then.
#[derive(Clone)]
pub struct LocalSelection {
    running: Senders,
    shown: Senders,
    /// Bumped per offer so a slow file listing for a stale one is dropped,
    /// and a read for an offer the clipboard has since moved past is refused.
    selection_gen: Rc<Cell<u64>>,
}

impl LocalSelection {
    pub fn new(
        running: impl Fn() -> Vec<UnboundedSender<ToWorker>> + 'static,
        shown: impl Fn() -> Vec<UnboundedSender<ToWorker>> + 'static,
    ) -> Self {
        Self {
            running: Rc::new(running),
            shown: Rc::new(shown),
            selection_gen: Rc::new(Cell::new(0)),
        }
    }

    /// The generation of the latest offer. An offer is current only while
    /// this still matches the one it was sent with.
    pub fn generation(&self) -> u64 {
        self.selection_gen.get()
    }

    /// Offer the current local selection again. The clipboard reports only
    /// changes, so a session shown after the last copy would otherwise have
    /// nothing to paste. Nothing is revoked here: the session shown before
    /// was revoked by the switch, and the clipboard has not changed.
    pub fn reoffer(&self) {
        let gen = self.begin();
        if let Some(cb) = clipboard() {
            if !holds_remote_offer(&cb) {
                self.read_formats(&cb, gen);
            }
        }
    }

    /// The local clipboard changed: revoke the current offer in every
    /// running session and start a new offer. Returns its generation, or
    /// None when nothing is to be offered because the server's offer holds
    /// the clipboard or no session runs.
    pub fn changed(&self, holds_remote: bool) -> Option<u64> {
        let gen = self.begin();
        revoke_for_change(&(self.running)(), holds_remote).then_some(gen)
    }

    fn offer(&self, cb: &gdk::Clipboard) {
        // Only our own proxy for the server's offer must not echo back;
        // a copy from another gliff widget is a real local change.
        if let Some(gen) = self.changed(holds_remote_offer(cb)) {
            self.read_formats(cb, gen);
        }
    }

    /// Start a new offer, superseding any still being prepared.
    pub fn begin(&self) -> u64 {
        let gen = self.selection_gen.get() + 1;
        self.selection_gen.set(gen);
        gen
    }

    fn read_formats(&self, cb: &gdk::Clipboard, gen: u64) {
        if (self.shown)().is_empty() {
            return;
        }
        let watch = self.clone();
        let mimes: Vec<String> = cb
            .formats()
            .mime_types()
            .iter()
            .map(|m| m.to_string())
            .collect();
        let cb = cb.clone();
        glib::MainContext::default().spawn_local(async move {
            let sensitive = sensitive_selection(&mimes);
            let files = if !sensitive && offers_files(&mimes) {
                local_files(&cb, &mimes).await
            } else {
                LocalFiles::default()
            };
            let mime_types = if sensitive {
                Vec::new()
            } else {
                forwardable_mimes(&mimes)
            };
            watch.finish(gen, mime_types, files);
        });
    }

    /// Send offer `gen` once its files are listed, unless a newer offer has
    /// started since.
    pub fn finish(&self, gen: u64, mime_types: Vec<String>, files: LocalFiles) {
        if self.selection_gen.get() != gen {
            return;
        }
        for tx in (self.shown)() {
            let _ = tx.send(ToWorker::LocalOffer {
                mime_types: mime_types.clone(),
                files: files.clone(),
                generation: gen,
            });
        }
    }
}

/// Withdraw the current local offer from every session, and say whether the
/// change is a local selection to offer. The revocation comes first, before
/// the file listing, and also when the server's offer took the clipboard:
/// then no new offer replaces the old one, and its files would stay
/// readable by every session.
fn revoke_for_change(txs: &[UnboundedSender<ToWorker>], holds_remote: bool) -> bool {
    for tx in txs {
        let _ = tx.send(ToWorker::RevokeLocal);
    }
    !holds_remote && !txs.is_empty()
}

/// List the files behind the local clipboard's file list, if readable. The
/// list is read as whichever file-list type the clipboard advertises; the
/// parser handles both bodies.
async fn local_files(cb: &gdk::Clipboard, mimes: &[String]) -> LocalFiles {
    let Some(list_mime) = mimes.iter().find(|m| is_file_mime(m)) else {
        return LocalFiles::default();
    };
    let Ok((stream, _)) = cb.read_future(&[list_mime], glib::Priority::DEFAULT).await else {
        return LocalFiles::default();
    };
    let mut body = Vec::new();
    while body.len() <= URI_LIST_MAX {
        match stream
            .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
            .await
        {
            Ok(b) if b.is_empty() => break,
            Ok(b) => body.extend_from_slice(&b),
            Err(_) => return LocalFiles::default(),
        }
    }
    if body.len() > URI_LIST_MAX {
        return LocalFiles::default();
    }
    let paths = parse_uri_list(&String::from_utf8_lossy(&body));
    if paths.is_empty() {
        return LocalFiles::default();
    }
    // The walk reads metadata for up to 10,000 entries, so it runs on its
    // own thread; blocking here would freeze rendering and input.
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(list_files(&paths));
    });
    match rx.await {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "clipboard files not offered");
            LocalFiles::default()
        }
        Err(_) => LocalFiles::default(),
    }
}

/// Stream the local clipboard's `mime_type` into `reply` for the server;
/// dropping `reply` marks the end. An error is sent as such. The server's
/// own offer is refused: reading it would fetch the item back from the
/// server, which would ask us for it again. `allowed` is asked when the
/// read starts: a request queued before a tab switch or a new copy must not
/// read what the clipboard holds now.
pub fn read_local(
    mime_type: String,
    reply: Sender<std::io::Result<Bytes>>,
    allowed: impl FnOnce() -> bool + 'static,
) {
    glib::MainContext::default().spawn_local(async move {
        if !allowed() {
            let error = std::io::Error::other("clipboard offer was withdrawn");
            let _ = reply.send(Err(error)).await;
            return;
        }
        let Some(cb) = clipboard() else {
            return;
        };
        if holds_remote_offer(&cb) {
            let error = std::io::Error::other("clipboard holds the server's offer");
            let _ = reply.send(Err(error)).await;
            return;
        }
        // A text request may name a flavour the local source does not
        // advertise; any text flavour it does have will do.
        let available: Vec<String> = cb
            .formats()
            .mime_types()
            .iter()
            .map(|m| m.to_string())
            .collect();
        if sensitive_selection(&available) {
            let _ = reply
                .send(Err(std::io::Error::other(
                    "clipboard selection is marked sensitive",
                )))
                .await;
            return;
        }
        let Some(actual) = resolve_mime(&mime_type, &available) else {
            let error = std::io::Error::other(format!("clipboard has no {mime_type}"));
            let _ = reply.send(Err(error)).await;
            return;
        };
        let stream = match cb.read_future(&[actual], glib::Priority::DEFAULT).await {
            Ok((stream, _)) => stream,
            Err(e) => {
                let _ = reply.send(Err(std::io::Error::other(e.to_string()))).await;
                return;
            }
        };
        loop {
            match stream
                .read_bytes_future(CHUNK, glib::Priority::DEFAULT)
                .await
            {
                Ok(b) if b.is_empty() => break,
                Ok(b) => {
                    if reply.send(Ok(Bytes::copy_from_slice(&b))).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = reply.send(Err(std::io::Error::other(e.to_string()))).await;
                    break;
                }
            }
        }
        let _ = stream.close_future(glib::Priority::DEFAULT).await;
    });
}

/// Make the server's offer the local selection, or clear ours for an empty
/// offer. The provider holds the session's sender weakly: it must not keep
/// a replaced session's input channel open, or that session never learns
/// the UI has moved on.
pub fn set_remote_offer(
    tx: &UnboundedSender<ToWorker>,
    serial: u32,
    mime_types: Vec<String>,
    files: Vec<ClipboardFile>,
) {
    let Some(cb) = clipboard() else {
        return;
    };
    let mimes = local_mimes_for_offer(&mime_types, !files.is_empty());
    if mimes.is_empty() {
        if holds_remote_offer(&cb) {
            let _ = cb.set_content(gdk::ContentProvider::NONE);
        }
        return;
    }
    let provider = RemoteProvider::new(tx.downgrade(), serial, mimes);
    if let Err(e) = cb.set_content(Some(&provider)) {
        tracing::warn!(error = %e, "clipboard proxy not set");
    }
}

glib::wrapper! {
    /// A content provider for the server's offer; each paste fetches the
    /// item from the server through the worker.
    pub struct RemoteProvider(ObjectSubclass<imp::RemoteProvider>)
        @extends gdk::ContentProvider;
}

impl RemoteProvider {
    fn new(tx: WeakUnboundedSender<ToWorker>, serial: u32, mime_types: Vec<String>) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        *imp.tx.borrow_mut() = Some(tx);
        imp.serial.set(serial);
        *imp.mime_types.borrow_mut() = mime_types;
        obj
    }
}

mod imp {
    use super::*;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;

    #[derive(Default)]
    pub struct RemoteProvider {
        pub tx: RefCell<Option<WeakUnboundedSender<ToWorker>>>,
        /// Serial of the offer this provider proxies. A paste echoes it, so
        /// one racing a newer offer is refused instead of served the wrong
        /// item.
        pub serial: Cell<u32>,
        pub mime_types: RefCell<Vec<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RemoteProvider {
        const NAME: &'static str = "GliffRemoteClipboard";
        type Type = super::RemoteProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for RemoteProvider {}

    impl ContentProviderImpl for RemoteProvider {
        fn formats(&self) -> gdk::ContentFormats {
            let mimes = self.mime_types.borrow();
            let refs: Vec<&str> = mimes.iter().map(String::as_str).collect();
            gdk::ContentFormats::new(&refs)
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let tx = self.tx.borrow().clone();
            let serial = self.serial.get();
            let mime_type = mime_type.to_string();
            let stream = stream.clone();
            Box::pin(async move {
                let failed = |m: String| glib::Error::new(gio::IOErrorEnum::Failed, &m);
                let tx = tx
                    .and_then(|w| w.upgrade())
                    .ok_or_else(|| failed("not connected".into()))?;
                let (sink, mut rx) = tokio::sync::mpsc::channel::<Bytes>(2);
                let (result_tx, result_rx) = oneshot::channel();
                tx.send(ToWorker::Fetch {
                    mime_type,
                    serial,
                    sink,
                    result: result_tx,
                })
                .map_err(|_| failed("worker gone".into()))?;
                while let Some(chunk) = rx.recv().await {
                    stream
                        .write_all_future(chunk, io_priority)
                        .await
                        .map_err(|(_, e)| e)?;
                }
                match result_rx.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(failed(e)),
                    Err(_) => Err(failed("transfer dropped".into())),
                }
            })
        }
    }
}

/// The transfer bars shown over the video while a paste from the server
/// takes a while: one row per job, with a cancel button. Empty rows are
/// removed, so the box is only visible while something is in flight.
pub struct TransferBars {
    list: gtk::Box,
    rows: RefCell<Bars<Row>>,
}

struct Row {
    root: gtk::Box,
    text: gtk::Label,
    bar: gtk::ProgressBar,
}

/// A row is keyed by the connection that ran its job and the job's id, as
/// every connection numbers its jobs from 1.
type BarKey = (u64, u32);

/// The rows of [`TransferBars`] by key, apart from the widgets so the
/// bookkeeping can be tested without a display.
struct Bars<R> {
    rows: HashMap<BarKey, R>,
    /// Rows the user closed whose job has not reported an end yet, so a
    /// report already on its way does not bring the row back.
    dismissed: HashSet<BarKey>,
}

impl<R> Default for Bars<R> {
    fn default() -> Self {
        Self {
            rows: HashMap::new(),
            dismissed: HashSet::new(),
        }
    }
}

impl<R> Bars<R> {
    /// The row to show a report in, added with `add` when there is none;
    /// `None` once the user closed it.
    fn row(&mut self, key: BarKey, add: impl FnOnce() -> R) -> Option<&mut R> {
        if self.dismissed.contains(&key) {
            return None;
        }
        Some(self.rows.entry(key).or_insert_with(add))
    }

    fn is_dismissed(&self, key: BarKey) -> bool {
        self.dismissed.contains(&key)
    }

    /// The job ended: its row goes, and so does any mark that it was closed.
    fn finish(&mut self, key: BarKey) -> Option<R> {
        self.dismissed.remove(&key);
        self.rows.remove(&key)
    }

    /// The user closed the row; it stays away until the job ends.
    fn dismiss(&mut self, key: BarKey) -> Option<R> {
        self.dismissed.insert(key);
        self.rows.remove(&key)
    }

    /// The connection ended, and with it every job it ran, reported or not.
    fn clear_connection(&mut self, conn: u64) -> Vec<R> {
        self.dismissed.retain(|&(c, _)| c != conn);
        let keys: Vec<BarKey> = self.rows.keys().filter(|k| k.0 == conn).copied().collect();
        keys.into_iter()
            .filter_map(|k| self.rows.remove(&k))
            .collect()
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// How long a failure stays on screen.
const FAILURE_LINGER: Duration = Duration::from_secs(6);

impl TransferBars {
    pub fn new() -> Self {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::End)
            .visible(false)
            .css_classes(["transfers"])
            .build();
        Self {
            list,
            rows: RefCell::default(),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.list
    }

    /// Show the state of job `id` of connection `conn`; `cancel` is called
    /// with the id when the user closes the row, which removes it at once.
    pub fn update(
        self: &Rc<Self>,
        conn: u64,
        id: u32,
        p: &Progress,
        cancel: impl Fn(u32) + 'static,
    ) {
        let key = (conn, id);
        let mut rows = self.rows.borrow_mut();
        match &p.state {
            State::Running => {
                let Some(row) = rows.row(key, || self.add_row(key, cancel)) else {
                    return;
                };
                row.text
                    .set_text(&format!("Pasting {}: {}", p.label, describe(p)));
                match p.percent() {
                    Some(pct) => row.bar.set_fraction(pct as f64 / 100.0),
                    None => row.bar.pulse(),
                }
            }
            State::Done | State::Cancelled => {
                let row = rows.finish(key);
                drop(rows);
                self.drop_rows(row);
            }
            State::Failed(e) => {
                if rows.is_dismissed(key) {
                    rows.finish(key);
                    return;
                }
                let Some(row) = rows.row(key, || self.add_row(key, cancel)) else {
                    return;
                };
                row.text
                    .set_text(&format!("Paste of {} failed: {e}", p.label));
                row.bar.set_visible(false);
                drop(rows);
                let this = self.clone();
                glib::timeout_add_local_once(FAILURE_LINGER, move || {
                    let row = this.rows.borrow_mut().finish(key);
                    this.drop_rows(row);
                });
            }
        }
    }

    /// Remove the rows of a connection that ended. Its jobs died with it
    /// without a final report, so nothing else would.
    pub fn clear_connection(&self, conn: u64) {
        let rows = self.rows.borrow_mut().clear_connection(conn);
        self.drop_rows(rows);
    }

    fn add_row(self: &Rc<Self>, key: BarKey, cancel: impl Fn(u32) + 'static) -> Row {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["transfer"])
            .build();
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        let text = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let bar = gtk::ProgressBar::new();
        column.append(&text);
        column.append(&bar);
        let button = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Cancel")
            .valign(gtk::Align::Center)
            .build();
        // The row goes whether or not the job is still there to cancel.
        let bars = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            cancel(key.1);
            if let Some(bars) = bars.upgrade() {
                let row = bars.rows.borrow_mut().dismiss(key);
                bars.drop_rows(row);
            }
        });
        root.append(&column);
        root.append(&button);
        self.list.append(&root);
        self.list.set_visible(true);
        Row { root, text, bar }
    }

    fn drop_rows(&self, rows: impl IntoIterator<Item = Row>) {
        for row in rows {
            self.list.remove(&row.root);
        }
        self.list.set_visible(!self.rows.borrow().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(bars: &Bars<&'static str>) -> Vec<BarKey> {
        let mut keys: Vec<BarKey> = bars.rows.keys().copied().collect();
        keys.sort();
        keys
    }

    #[test]
    fn jobs_of_two_connections_keep_their_own_rows() {
        let mut bars = Bars::default();
        bars.row((1, 1), || "old");
        bars.row((2, 1), || "new");
        assert_eq!(shown(&bars), [(1, 1), (2, 1)]);
        assert_eq!(bars.row((2, 1), || "replaced").map(|r| *r), Some("new"));
        assert_eq!(bars.finish((2, 1)), Some("new"));
        assert_eq!(shown(&bars), [(1, 1)]);
    }

    #[test]
    fn an_ended_connection_takes_only_its_rows() {
        let mut bars = Bars::default();
        bars.row((1, 1), || "a");
        bars.row((1, 2), || "b");
        bars.row((2, 1), || "c");
        let mut gone = bars.clear_connection(1);
        gone.sort();
        assert_eq!(gone, ["a", "b"]);
        assert_eq!(shown(&bars), [(2, 1)]);
        assert!(bars.clear_connection(1).is_empty());
    }

    #[test]
    fn a_closed_row_stays_closed_until_its_job_ends() {
        let mut bars = Bars::default();
        bars.row((1, 1), || "a");
        assert_eq!(bars.dismiss((1, 1)), Some("a"));
        // A progress report already queued must not bring it back.
        assert!(bars.row((1, 1), || "again").is_none());
        assert!(bars.is_empty());
        assert_eq!(bars.finish((1, 1)), None);
        assert!(!bars.is_dismissed((1, 1)));
    }

    #[test]
    fn a_row_with_no_job_left_can_still_be_closed() {
        // The worker that ran the job is gone, so no report will follow.
        let mut bars = Bars::default();
        bars.row((1, 7), || "stale");
        assert_eq!(bars.dismiss((1, 7)), Some("stale"));
        assert!(bars.is_empty());
    }

    #[test]
    fn closing_marks_go_with_their_connection() {
        let mut bars = Bars::default();
        bars.row((1, 1), || "a");
        bars.row((2, 1), || "b");
        bars.dismiss((1, 1));
        bars.dismiss((2, 1));
        bars.clear_connection(1);
        assert!(!bars.is_dismissed((1, 1)));
        assert!(bars.is_dismissed((2, 1)));
    }

    #[test]
    fn every_session_is_revoked_when_the_servers_offer_takes_the_clipboard() {
        for holds_remote in [true, false] {
            let (a_tx, mut a_rx) = tokio::sync::mpsc::unbounded_channel();
            let (b_tx, mut b_rx) = tokio::sync::mpsc::unbounded_channel();
            let offer = revoke_for_change(&[a_tx, b_tx], holds_remote);
            assert_eq!(offer, !holds_remote);
            for rx in [&mut a_rx, &mut b_rx] {
                assert!(matches!(rx.try_recv(), Ok(ToWorker::RevokeLocal)));
                assert!(rx.try_recv().is_err());
            }
        }
        assert!(!revoke_for_change(&[], false));
    }

    #[test]
    #[ignore = "requires an isolated Wayland session; run by scripts/e2e.sh"]
    fn password_manager_selection_cannot_be_offered_or_read() {
        gtk::init().unwrap();
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let cb = clipboard().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Every session is running, and the first one is shown.
        let sessions = Rc::new(RefCell::new(vec![tx]));
        let running = sessions.clone();
        let shown = sessions.clone();
        watch_local(
            move || running.borrow().clone(),
            move || shown.borrow().first().cloned().into_iter().collect(),
        );
        let provider = gdk::ContentProvider::new_union(&[
            gdk::ContentProvider::for_bytes(
                "text/plain",
                &glib::Bytes::from_static(b"private-password"),
            ),
            gdk::ContentProvider::for_bytes(
                "text/uri-list",
                &glib::Bytes::from_static(b"file:///private-secret\r\n"),
            ),
            gdk::ContentProvider::for_bytes(
                "x-kde-passwordManagerHint",
                &glib::Bytes::from_static(b"secret"),
            ),
        ]);
        cb.set_content(Some(&provider)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut revoked = false;
        let offer = loop {
            context.iteration(false);
            match rx.try_recv() {
                Ok(ToWorker::RevokeLocal) => revoked = true,
                Ok(offer) => break offer,
                Err(_) => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sensitive offer was not cleared"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(revoked, "the previous offer was not revoked first");
        match offer {
            ToWorker::LocalOffer {
                mime_types, files, ..
            } => {
                assert!(mime_types.is_empty());
                assert!(files.entries.is_empty());
            }
            _ => panic!("expected a cleared offer"),
        }
        // A request racing the cleared offer must not read the live secret.
        let (reply, mut read) = tokio::sync::mpsc::channel(2);
        read_local("text/plain".into(), reply, || true);
        let result = loop {
            context.iteration(false);
            if let Ok(result) = read.try_recv() {
                break result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "clipboard read was not refused"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(result.is_err());

        // The server's offer taking the clipboard must revoke the local
        // files from every session, with no new local offer after it.
        let (a_tx, mut a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = tokio::sync::mpsc::unbounded_channel();
        *sessions.borrow_mut() = vec![a_tx.clone(), b_tx];
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let uri_list = glib::Bytes::from_owned(format!("file://{file}\r\n").into_bytes());
        cb.set_content(Some(&gdk::ContentProvider::for_bytes(
            "text/uri-list",
            &uri_list,
        )))
        .unwrap();
        // Everything session a receives, replayed into a Bridge below.
        let mut a_msgs = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            context.iteration(false);
            while let Ok(msg) = a_rx.try_recv() {
                a_msgs.push(msg);
            }
            if a_msgs.iter().any(
                |m| matches!(m, ToWorker::LocalOffer { files, .. } if files.entries.len() == 1),
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "local file offer was not sent"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Only the shown session a is offered the copy. Session b is only
        // revoked.
        while let Ok(msg) = b_rx.try_recv() {
            assert!(
                !matches!(msg, ToWorker::LocalOffer { .. }),
                "session b got a local offer while hidden"
            );
        }
        let offered = a_msgs.len();

        set_remote_offer(&a_tx, 7, vec!["text/plain".into()], Vec::new());
        assert!(holds_remote_offer(&cb));
        let mut b_msgs = Vec::new();
        // Keep pumping past the revocations, so a late offer would show.
        let settle = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < settle {
            context.iteration(false);
            while let Ok(msg) = a_rx.try_recv() {
                a_msgs.push(msg);
            }
            while let Ok(msg) = b_rx.try_recv() {
                b_msgs.push(msg);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Withdraw our proxy before the checks can fail and end the test, so
        // the compositor has no selection when the next e2e step copies. No
        // worker answers the proxy here, so a paste from it would hang.
        cb.set_content(gdk::ContentProvider::NONE).unwrap();
        let settle = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < settle {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        for (name, after) in [("a", &a_msgs[offered..]), ("b", &b_msgs[..])] {
            assert!(
                after.iter().any(|m| matches!(m, ToWorker::RevokeLocal)),
                "session {name} was not revoked when the server's offer took the clipboard"
            );
            assert!(
                !after
                    .iter()
                    .any(|m| matches!(m, ToWorker::LocalOffer { .. })),
                "session {name} got a local offer for the server's own selection"
            );
        }

        // A Bridge fed what session a received refuses the old offer's file.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            use crate::clipboard::Bridge;
            use gliff_proto::{ClipboardItem, ClipboardMsg};
            use gliff_transport::clipboard::{outbound_channel, Side, Transfers};
            let (out, mut wire) = outbound_channel();
            let (status_tx, _status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            let mut serial = None;
            for msg in a_msgs {
                let is_offer = matches!(msg, ToWorker::LocalOffer { .. });
                bridge.on_ui(msg);
                if is_offer {
                    match wire.recv().await.unwrap().0 {
                        ClipboardMsg::Offer { serial: s, .. } => serial = Some(s),
                        other => panic!("expected an offer, got {other:?}"),
                    }
                }
            }
            bridge.on_peer_msg(
                ClipboardMsg::Request {
                    id: 2,
                    serial: serial.expect("no local offer reached the bridge"),
                    item: ClipboardItem::File(0),
                },
                Bytes::new(),
            );
            let answer = tokio::time::timeout(Duration::from_secs(1), wire.recv())
                .await
                .expect("file request was neither served nor refused")
                .unwrap();
            assert!(
                matches!(answer.0, ClipboardMsg::Abort { id: 2 }),
                "old offer's file was served after the takeover: {:?}",
                answer.0
            );
        });
    }
}
