//! The network worker's half of the clipboard: joins the transfer engine to
//! the GTK thread, which owns `gdk::Clipboard`. Bytes cross between the two
//! threads through bounded channels, so the ack window applies end to end.
//!
//! Server to local: the server's Offer goes to the UI as a status, which puts
//! a proxy provider on the local clipboard. When an application pastes, the
//! provider sends a [`ToWorker::Fetch`] with a channel; the item is fetched
//! from the server into that channel. A file paste spools every file first,
//! once per offer, and answers with a URI list into the spool.
//!
//! Local to server: the UI reports a new local selection as
//! [`ToWorker::LocalOffer`]. When the server requests a mime type the UI is
//! asked to read it ([`Status::ClipboardRead`]) into a channel that is served
//! to the engine; a file is served from disk here. Every local change first
//! revokes the current offer ([`ToWorker::RevokeLocal`]), so nothing copied
//! earlier stays readable while the next offer is built, or once the
//! server's offer holds the clipboard and no local offer follows.

use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender as StdSender;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, OnceCell};

use gliff_proto::clipboard::{file_list_body, is_file_mime, resolve_mime, MAX_ITEM};
use gliff_proto::{ClientMsg, ClipboardFile, ClipboardItem, ClipboardMsg};
use gliff_transport::clipboard::files::{
    fetch_files, open_source, retire, write_body, LocalFiles, Spool,
};
use gliff_transport::clipboard::progress::{describe_files, describe_mime, Jobs};
use gliff_transport::clipboard::{Event, TransferError, Transfers};

use crate::net::Status;

/// What the UI thread sends the worker.
pub enum ToWorker {
    Send(ClientMsg),
    /// Display pixels per stream pixel the decoder should write, so the
    /// picture is drawn 1:1 at an integer scale (see `paintable::layout`).
    Zoom(u32),
    /// The local clipboard changed: refuse requests against the current
    /// offer and stop serving it. A `LocalOffer` may follow.
    RevokeLocal,
    /// The local clipboard changed to these forwardable mime types and files.
    /// An empty offer withdraws the previous one: it takes a fresh serial,
    /// so requests against the old one are refused.
    LocalOffer {
        mime_types: Vec<String>,
        files: LocalFiles,
        /// The UI's offer generation, handed back with each read so the UI
        /// can refuse one for an offer it has since moved past.
        generation: u64,
    },
    /// An application pastes `mime_type` from the server's offer with
    /// `serial`: stream it into `sink`, then report the outcome.
    Fetch {
        mime_type: String,
        serial: u32,
        sink: mpsc::Sender<Bytes>,
        result: oneshot::Sender<Result<(), String>>,
    },
    /// The user pressed cancel on a transfer reported as `id`.
    CancelTransfer(u32),
}

#[derive(Default)]
struct RemoteOffer {
    serial: u32,
    files: Vec<ClipboardFile>,
    spooled: Rc<OnceCell<Vec<PathBuf>>>,
    spool: Option<Rc<Spool>>,
}

pub struct Bridge {
    transfers: Transfers,
    jobs: Jobs,
    status: StdSender<Status>,
    /// None while no local selection is offered: before the first one and
    /// after a revocation. Serial zero is a valid wire value, so it must not
    /// stand in for an absent offer. Shared with file opens in flight, which
    /// check it again before serving.
    local_serial: Rc<Cell<Option<u32>>>,
    /// The last serial offered, kept across revocations so a new offer does
    /// not restart at 1 and match a serial the peer still holds.
    last_serial: Cell<u32>,
    local_mimes: RefCell<Vec<String>>,
    local_files: RefCell<LocalFiles>,
    local_generation: Cell<u64>,
    remote: RefCell<RemoteOffer>,
}

impl Bridge {
    pub fn new(transfers: Transfers, status: StdSender<Status>) -> Self {
        let report_to = status.clone();
        Self {
            transfers,
            jobs: Jobs::new(move |id, progress| {
                let _ = report_to.send(Status::ClipboardTransfer { id, progress });
            }),
            status,
            local_serial: Rc::new(Cell::new(None)),
            last_serial: Cell::new(0),
            local_mimes: RefCell::default(),
            local_files: RefCell::default(),
            local_generation: Cell::new(0),
            remote: RefCell::default(),
        }
    }

    pub fn on_peer_msg(&self, msg: ClipboardMsg, payload: Bytes) {
        match self.transfers.on_msg(msg, payload) {
            Some(Event::Offer {
                serial,
                mime_types,
                files,
            }) => {
                let previous = std::mem::replace(
                    &mut *self.remote.borrow_mut(),
                    RemoteOffer {
                        serial,
                        files: files.clone(),
                        ..RemoteOffer::default()
                    },
                );
                if let Some(spool) = previous.spool {
                    retire(spool);
                }
                let _ = self.status.send(Status::ClipboardOffer {
                    serial,
                    mime_types,
                    files,
                });
            }
            Some(Event::Request { id, serial, item }) => self.on_request(id, serial, item),
            None => {}
        }
    }

    fn on_request(&self, id: u32, serial: u32, item: ClipboardItem) {
        if self.local_serial.get() != Some(serial) {
            tracing::debug!(id, serial, "clipboard request against a replaced offer");
            self.transfers.refuse(id);
            return;
        }
        match item {
            ClipboardItem::Mime(mime_type) => {
                // The UI reads the live clipboard, which may expose types
                // deliberately omitted from the offer (such as URI lists).
                if resolve_mime(&mime_type, &self.local_mimes.borrow()).is_none() {
                    self.transfers.refuse(id);
                    return;
                }
                let (reply, rx) = mpsc::channel::<io::Result<Bytes>>(2);
                if self
                    .status
                    .send(Status::ClipboardRead {
                        mime_type,
                        generation: self.local_generation.get(),
                        reply,
                    })
                    .is_err()
                {
                    self.transfers.refuse(id);
                    return;
                }
                self.transfers.serve(id, rx);
            }
            ClipboardItem::File(_) => {
                let Some(path) = self.local_files.borrow().path_for(&item).map(PathBuf::from)
                else {
                    self.transfers.refuse(id);
                    return;
                };
                let transfers = self.transfers.clone();
                let current = self.local_serial.clone();
                tokio::task::spawn_local(async move {
                    let opened = open_source(&path).await;
                    // A revocation while the file opened has nothing to
                    // abort yet, so it is caught here instead.
                    if current.get() != Some(serial) {
                        tracing::debug!(id, serial, "clipboard offer revoked while opening a file");
                        transfers.refuse(id);
                        return;
                    }
                    match opened {
                        Ok(src) => transfers.serve(id, src),
                        Err(e) => {
                            tracing::warn!(id, path = %path.display(), error = %e, "clipboard file open failed");
                            transfers.refuse(id);
                        }
                    }
                });
            }
        }
    }

    /// A clipboard command from the UI; `Send` is handled by the caller.
    pub fn on_ui(&self, cmd: ToWorker) {
        match cmd {
            ToWorker::Send(_) | ToWorker::Zoom(_) => {}
            ToWorker::RevokeLocal => {
                self.local_serial.set(None);
                self.local_mimes.borrow_mut().clear();
                *self.local_files.borrow_mut() = LocalFiles::default();
                self.transfers.abort_serving();
            }
            ToWorker::LocalOffer {
                mime_types,
                files,
                generation,
            } => {
                let serial = self.last_serial.get().wrapping_add(1);
                self.last_serial.set(serial);
                self.local_serial.set(Some(serial));
                self.local_generation.set(generation);
                *self.local_mimes.borrow_mut() = mime_types.clone();
                let entries = files.entries.clone();
                *self.local_files.borrow_mut() = files;
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    let _ = transfers
                        .send(ClipboardMsg::Offer {
                            serial,
                            mime_types,
                            files: entries,
                        })
                        .await;
                });
            }
            ToWorker::Fetch {
                mime_type,
                serial,
                sink,
                result,
            } => self.fetch(mime_type, serial, sink, result),
            ToWorker::CancelTransfer(id) => self.jobs.cancel(id),
        }
    }

    fn fetch(
        &self,
        mime_type: String,
        serial: u32,
        sink: mpsc::Sender<Bytes>,
        result: oneshot::Sender<Result<(), String>>,
    ) {
        let transfers = self.transfers.clone();
        if !is_file_mime(&mime_type) {
            self.jobs
                .run(describe_mime(&mime_type), None, |meter| async move {
                    let r = transfers
                        .fetch(
                            ClipboardItem::Mime(mime_type),
                            serial,
                            meter.wrap(sink),
                            Some(MAX_ITEM),
                        )
                        .await;
                    let _ = result.send(r.as_ref().map(|_| ()).map_err(|e| e.to_string()));
                    r.map(|_| ())
                });
            return;
        }
        let (files, spooled, spool) = {
            let mut remote = self.remote.borrow_mut();
            if remote.serial != serial {
                let _ = result.send(Err("the offer was replaced".into()));
                return;
            }
            if remote.files.is_empty() {
                let _ = result.send(Err("no files offered".into()));
                return;
            }
            if remote.spool.is_none() {
                match Spool::create() {
                    Ok(s) => remote.spool = Some(Rc::new(s)),
                    Err(e) => {
                        let _ = result.send(Err(format!("clipboard spool dir: {e}")));
                        return;
                    }
                }
            }
            (
                remote.files.clone(),
                remote.spooled.clone(),
                remote.spool.clone().expect("just created"),
            )
        };
        let (label, total) = describe_files(&files);
        self.jobs.run(label, total, |meter| async move {
            let r = match spooled
                .get_or_try_init(|| fetch_files(&transfers, &files, serial, &spool, &meter))
                .await
            {
                Ok(paths) => {
                    let body = file_list_body(&mime_type, paths).unwrap_or_default();
                    write_body(sink, body).await.map_err(TransferError::from)
                }
                Err(e) => Err(e),
            };
            let _ = result.send(r.as_ref().map(|_| ()).map_err(|e| e.to_string()));
            r
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_transport::clipboard::{outbound_channel, Side};

    #[test]
    fn a_request_before_any_local_offer_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            for (id, item) in [
                (2, ClipboardItem::Mime("text/plain".into())),
                (4, ClipboardItem::File(0)),
            ] {
                bridge.on_peer_msg(
                    ClipboardMsg::Request {
                        id,
                        serial: 0,
                        item,
                    },
                    Bytes::new(),
                );
                let refused = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                    .await
                    .expect("request without an offer was not refused")
                    .unwrap();
                assert!(matches!(refused.0, ClipboardMsg::Abort { id: got } if got == id));
                assert!(status_rx.try_recv().is_err());
            }
        });
    }

    #[test]
    fn requests_are_limited_to_the_current_offers_mime_types() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            for (serial, offered, requested, allowed) in [
                (1, "text/plain", "text/uri-list", false),
                (2, "image/png", "text/plain", false),
                (3, "text/plain", "UTF8_STRING", true),
                (4, "image/png", "image/png", true),
            ] {
                bridge.on_ui(ToWorker::LocalOffer {
                    mime_types: vec![offered.into()],
                    files: LocalFiles::default(),
                    generation: 0,
                });
                let offer = rx.recv().await.unwrap();
                assert!(
                    matches!(offer.0, ClipboardMsg::Offer { serial: got, .. } if got == serial)
                );
                let id = serial * 2;
                bridge.on_peer_msg(
                    ClipboardMsg::Request {
                        id,
                        serial,
                        item: ClipboardItem::Mime(requested.into()),
                    },
                    Bytes::new(),
                );
                if allowed {
                    assert!(matches!(
                        status_rx.try_recv().unwrap(),
                        Status::ClipboardRead { mime_type, .. } if mime_type == requested
                    ));
                    let sent = rx.recv().await.unwrap();
                    assert!(matches!(
                        sent.0,
                        ClipboardMsg::Data { id: got, done: true, .. } if got == id
                    ));
                } else {
                    let refused =
                        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                            .await
                            .expect("unoffered mime type was not refused")
                            .unwrap();
                    assert!(matches!(refused.0, ClipboardMsg::Abort { id: got } if got == id));
                    assert!(status_rx.try_recv().is_err());
                }
            }
        });
    }

    #[test]
    fn an_empty_offer_revokes_the_previous_one() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            bridge.on_ui(ToWorker::LocalOffer {
                mime_types: vec!["text/plain".into()],
                files: LocalFiles::default(),
                generation: 0,
            });
            let offered = rx.recv().await.unwrap();
            assert!(matches!(offered.0, ClipboardMsg::Offer { serial: 1, .. }));
            bridge.on_ui(ToWorker::LocalOffer {
                mime_types: Vec::new(),
                files: LocalFiles::default(),
                generation: 0,
            });
            let revoked = rx.recv().await.unwrap();
            assert!(matches!(
                revoked.0,
                ClipboardMsg::Offer { serial: 2, ref mime_types, ref files }
                    if mime_types.is_empty() && files.is_empty()
            ));
            for (id, serial) in [(2, 1), (4, 2)] {
                bridge.on_peer_msg(
                    ClipboardMsg::Request {
                        id,
                        serial,
                        item: ClipboardItem::Mime("text/plain".into()),
                    },
                    Bytes::new(),
                );
                let refused = rx.recv().await.unwrap();
                assert!(matches!(refused.0, ClipboardMsg::Abort { id: got } if got == id));
            }
            assert!(status_rx.try_recv().is_err());
        });
    }

    #[test]
    fn a_request_against_a_replaced_offer_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            bridge.on_ui(ToWorker::LocalOffer {
                mime_types: vec!["text/plain".into()],
                files: LocalFiles::default(),
                generation: 7,
            });
            let offered = rx.recv().await.unwrap();
            assert!(matches!(offered.0, ClipboardMsg::Offer { serial: 1, .. }));
            bridge.on_peer_msg(
                ClipboardMsg::Request {
                    id: 2,
                    serial: 0,
                    item: ClipboardItem::Mime("text/plain".into()),
                },
                Bytes::new(),
            );
            let refused = rx.recv().await.unwrap();
            assert!(matches!(refused.0, ClipboardMsg::Abort { id: 2 }));
            assert!(status_rx.try_recv().is_err());
            bridge.on_peer_msg(
                ClipboardMsg::Request {
                    id: 4,
                    serial: 1,
                    item: ClipboardItem::Mime("text/plain".into()),
                },
                Bytes::new(),
            );
            // The read carries the UI's generation for the offer, so the UI
            // can refuse it if the offer has been withdrawn since.
            assert!(matches!(
                status_rx.try_recv().unwrap(),
                Status::ClipboardRead { generation: 7, .. }
            ));
        });
    }

    /// A local offer of one file that exists while the tests run.
    fn one_file() -> LocalFiles {
        let path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        LocalFiles {
            entries: vec![ClipboardFile {
                path: "Cargo.toml".into(),
                size: std::fs::metadata(&path).unwrap().len(),
                dir: false,
            }],
            paths: vec![path],
        }
    }

    fn offer_file(bridge: &Bridge) {
        bridge.on_ui(ToWorker::LocalOffer {
            mime_types: vec!["text/uri-list".into()],
            files: one_file(),
            generation: 0,
        });
    }

    fn request_file(bridge: &Bridge, id: u32, serial: u32) {
        bridge.on_peer_msg(
            ClipboardMsg::Request {
                id,
                serial,
                item: ClipboardItem::File(0),
            },
            Bytes::new(),
        );
    }

    #[test]
    fn a_file_of_the_current_offer_is_served() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, _status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            offer_file(&bridge);
            let offered = rx.recv().await.unwrap();
            assert!(matches!(offered.0, ClipboardMsg::Offer { serial: 1, .. }));
            request_file(&bridge, 2, 1);
            let sent = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("offered file was not served")
                .unwrap();
            assert!(matches!(sent.0, ClipboardMsg::Data { id: 2, .. }));
        });
    }

    #[test]
    fn a_file_request_after_a_revocation_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, _status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            offer_file(&bridge);
            let offered = rx.recv().await.unwrap();
            assert!(matches!(offered.0, ClipboardMsg::Offer { serial: 1, .. }));
            // The server's offer took the clipboard: no new local offer
            // follows, so the revocation alone must close the old one.
            bridge.on_ui(ToWorker::RevokeLocal);
            request_file(&bridge, 2, 1);
            let refused = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("file request after a revocation was not refused")
                .unwrap();
            assert!(matches!(refused.0, ClipboardMsg::Abort { id: 2 }));
        });
    }

    #[test]
    fn a_revocation_while_a_file_opens_refuses_the_serve() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, _status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            offer_file(&bridge);
            let offered = rx.recv().await.unwrap();
            assert!(matches!(offered.0, ClipboardMsg::Offer { serial: 1, .. }));
            // The open is spawned but cannot run before this task yields,
            // so the revocation lands while it is in flight.
            request_file(&bridge, 2, 1);
            bridge.on_ui(ToWorker::RevokeLocal);
            let refused = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("file opened before a revocation was neither served nor refused")
                .unwrap();
            assert!(matches!(refused.0, ClipboardMsg::Abort { id: 2 }));
        });
    }

    #[test]
    fn a_revocation_aborts_transfers_being_served() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            bridge.on_ui(ToWorker::LocalOffer {
                mime_types: vec!["text/plain".into()],
                files: LocalFiles::default(),
                generation: 0,
            });
            let offered = rx.recv().await.unwrap();
            assert!(matches!(offered.0, ClipboardMsg::Offer { serial: 1, .. }));
            bridge.on_peer_msg(
                ClipboardMsg::Request {
                    id: 2,
                    serial: 1,
                    item: ClipboardItem::Mime("text/plain".into()),
                },
                Bytes::new(),
            );
            // Hold the UI's side open without data, so the serve is pending.
            let Status::ClipboardRead { reply, .. } = status_rx.try_recv().unwrap() else {
                panic!("expected a clipboard read");
            };
            tokio::task::yield_now().await;
            bridge.on_ui(ToWorker::RevokeLocal);
            let aborted = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("serve in flight was not aborted")
                .unwrap();
            assert!(matches!(aborted.0, ClipboardMsg::Abort { id: 2 }));
            // The serve dropped its source, so the UI stops reading the
            // clipboard for it.
            tokio::time::timeout(std::time::Duration::from_secs(1), reply.closed())
                .await
                .expect("aborted serve still holds the clipboard read");
        });
    }

    #[test]
    fn serials_keep_increasing_after_a_revocation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, _status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            for serial in 1..=3 {
                offer_file(&bridge);
                let offered = rx.recv().await.unwrap();
                assert!(
                    matches!(offered.0, ClipboardMsg::Offer { serial: got, .. } if got == serial)
                );
                bridge.on_ui(ToWorker::RevokeLocal);
            }
            // The first offer's serial was not handed out again, so a
            // request still holding it is refused.
            offer_file(&bridge);
            assert!(matches!(
                rx.recv().await.unwrap().0,
                ClipboardMsg::Offer { serial: 4, .. }
            ));
            request_file(&bridge, 2, 1);
            assert!(matches!(
                rx.recv().await.unwrap().0,
                ClipboardMsg::Abort { id: 2 }
            ));
            request_file(&bridge, 4, 4);
            assert!(matches!(
                rx.recv().await.unwrap().0,
                ClipboardMsg::Data { id: 4, .. }
            ));
        });
    }
}
