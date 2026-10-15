//! The GTK thread's half of the clipboard: watches the local `gdk::Clipboard`
//! for changes to offer to the server, reads a local item when the server
//! requests one, and proxies the server's offer as a lazy content provider
//! whose bytes are fetched only when an application pastes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
    gdk::Display::default().map(|d| d.clipboard())
}

fn sensitive_selection(mimes: &[String]) -> bool {
    mimes.iter().any(|m| m == "x-kde-passwordManagerHint")
}

fn holds_remote_offer(cb: &gdk::Clipboard) -> bool {
    cb.content().is_some_and(|p| p.is::<RemoteProvider>())
}

/// Report every change of the local clipboard to the workers, except changes
/// made by our own proxy provider. Every running session gets the offer, so
/// any of them can paste it after a switch; nothing is sent until one does.
pub fn watch_local(senders: impl Fn() -> Vec<UnboundedSender<ToWorker>> + 'static) {
    let Some(cb) = clipboard() else {
        return;
    };
    let senders = Rc::new(senders);
    // Bumped per change so a slow file listing for a stale one is dropped.
    let selection_gen = Rc::new(Cell::new(0u64));
    cb.connect_changed(move |cb| {
        // Only our own proxy for the server's offer must not echo back;
        // a copy from another gliff widget is a real local change.
        let gen = selection_gen.get() + 1;
        selection_gen.set(gen);
        if holds_remote_offer(cb) {
            return;
        }
        let txs = senders();
        if txs.is_empty() {
            return;
        }
        let selection_gen = selection_gen.clone();
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
            if selection_gen.get() != gen {
                return;
            }
            for tx in txs {
                let _ = tx.send(ToWorker::LocalOffer {
                    mime_types: if sensitive {
                        Vec::new()
                    } else {
                        forwardable_mimes(&mimes)
                    },
                    files: files.clone(),
                });
            }
        });
    });
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
/// server, which would ask us for it again.
pub fn read_local(mime_type: String, reply: Sender<std::io::Result<Bytes>>) {
    glib::MainContext::default().spawn_local(async move {
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
    rows: RefCell<HashMap<u32, Row>>,
}

struct Row {
    root: gtk::Box,
    text: gtk::Label,
    bar: gtk::ProgressBar,
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

    /// Show the state of job `id`; `cancel` is called with the id when the
    /// user presses the row's button.
    pub fn update(self: &Rc<Self>, id: u32, p: &Progress, cancel: impl Fn(u32) + 'static) {
        match &p.state {
            State::Running => {
                let mut rows = self.rows.borrow_mut();
                let row = rows.entry(id).or_insert_with(|| self.add_row(id, cancel));
                row.text
                    .set_text(&format!("Pasting {}: {}", p.label, describe(p)));
                match p.percent() {
                    Some(pct) => row.bar.set_fraction(pct as f64 / 100.0),
                    None => row.bar.pulse(),
                }
            }
            State::Done | State::Cancelled => self.remove(id),
            State::Failed(e) => {
                let mut rows = self.rows.borrow_mut();
                let row = rows.entry(id).or_insert_with(|| self.add_row(id, cancel));
                row.text
                    .set_text(&format!("Paste of {} failed: {e}", p.label));
                row.bar.set_visible(false);
                drop(rows);
                let this = self.clone();
                glib::timeout_add_local_once(FAILURE_LINGER, move || this.remove(id));
            }
        }
    }

    fn add_row(&self, id: u32, cancel: impl Fn(u32) + 'static) -> Row {
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
        button.connect_clicked(move |_| cancel(id));
        root.append(&column);
        root.append(&button);
        self.list.append(&root);
        self.list.set_visible(true);
        Row { root, text, bar }
    }

    fn remove(&self, id: u32) {
        let mut rows = self.rows.borrow_mut();
        if let Some(row) = rows.remove(&id) {
            self.list.remove(&row.root);
        }
        self.list.set_visible(!rows.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an isolated Wayland session; run by scripts/e2e.sh"]
    fn password_manager_selection_cannot_be_offered_or_read() {
        gtk::init().unwrap();
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let cb = clipboard().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        watch_local(move || vec![tx.clone()]);
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
        let offer = loop {
            context.iteration(false);
            if let Ok(offer) = rx.try_recv() {
                break offer;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sensitive offer was not cleared"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        match offer {
            ToWorker::LocalOffer { mime_types, files } => {
                assert!(mime_types.is_empty());
                assert!(files.entries.is_empty());
            }
            _ => panic!("expected a cleared offer"),
        }
        // A request racing the cleared offer must not read the live secret.
        let (reply, mut read) = tokio::sync::mpsc::channel(2);
        read_local("text/plain".into(), reply);
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
    }
}
