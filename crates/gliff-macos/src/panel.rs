//! The connection window ("choose") and the settings window ("settings"):
//! one JSON conversation with the native helper, wherever the window lives.
//!
//! While a viewer is running, the window opens inside it ([`Panel::Viewer`]):
//! it is one of the viewer's own windows, so it shares the app's Dock icon,
//! ⌘-Tab entry and menu bar, and the connection window comes up as a sheet on
//! the front tab. The client gives it an id from the viewer's window ids;
//! its events come back through the viewer's router with `"window": id`
//! (see `client::Hub`), and commands for it follow a `TARGET` packet.
//!
//! Before a viewer exists (the connection window at launch) it is a helper
//! process of its own ([`Panel::Process`]), as before.
//!
//! Version: 1.0.0

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc;

/// Events from the viewer, by window id (tabs and panels alike).
pub type Routes = Rc<RefCell<HashMap<u32, mpsc::UnboundedSender<Result<Value>>>>>;

/// What a running viewer lends to panels: its packet queue (window id, kind,
/// bytes), its event routes and its next free window id.
#[derive(Clone)]
pub struct ViewerHost {
    pub out: mpsc::Sender<(u32, u8, Vec<u8>)>,
    pub routes: Routes,
    pub next: Rc<Cell<u32>>,
}

thread_local! {
    //= The client runs on one thread (a LocalSet), so the running viewer is
    //= a thread-local: every caller of Panel::open sees it without plumbing.
    static VIEWER: RefCell<Option<ViewerHost>> = const { RefCell::new(None) };
}

/// Register the running viewer (`Hub::open`), or forget it when it is gone.
pub fn set_viewer(host: Option<ViewerHost>) {
    VIEWER.with(|v| *v.borrow_mut() = host);
}

fn viewer() -> Option<ViewerHost> {
    VIEWER.with(|v| v.borrow().clone())
}

/// What a panel does with one event routed to it by the viewer.
#[derive(Debug, PartialEq)]
pub enum Routed {
    /// The panel's own event.
    Keep(Value),
    /// Sent to every window but not meant for panels (keyboard layouts).
    Skip,
    /// The viewer is quitting.
    Gone,
}

pub fn classify(ev: Value) -> Routed {
    match ev["type"].as_str() {
        Some("keyboard_layout") => Routed::Skip,
        Some("bye") => Routed::Gone,
        _ => Routed::Keep(ev),
    }
}

/// The viewer command that opens a panel; `args` are the helper's arguments.
pub fn open_command(kind: &str, args: &[String]) -> Value {
    json!({"type": "open_panel", "kind": kind, "args": args})
}

/// One open connection or settings window.
//= Only one or two exist at a time, so the variants' sizes do not matter.
#[allow(clippy::large_enum_variant)]
pub enum Panel {
    #[cfg(target_os = "macos")]
    Process {
        _child: tokio::process::Child,
        rd: tokio::process::ChildStdout,
        wr: tokio::process::ChildStdin,
    },
    Viewer {
        id: u32,
        host: ViewerHost,
        rx: mpsc::UnboundedReceiver<Result<Value>>,
    },
}

impl Panel {
    /// Open `kind` ("choose" or "settings") in the running viewer, else as
    /// its own helper process.
    pub async fn open(kind: &str, args: &[String]) -> Result<Self> {
        if let Some(host) = viewer() {
            let id = host.next.get();
            host.next.set(id + 1);
            let (tx, rx) = mpsc::unbounded_channel();
            host.routes.borrow_mut().insert(id, tx);
            let bytes = serde_json::to_vec(&open_command(kind, args))?;
            if host
                .out
                .send((id, crate::native::EVENT, bytes))
                .await
                .is_err()
            {
                host.routes.borrow_mut().remove(&id);
                bail!("the viewer is gone");
            }
            tracing::debug!(kind, id, "panel opened in the viewer");
            return Ok(Panel::Viewer { id, host, rx });
        }
        Self::process(kind, args)
    }

    #[cfg(target_os = "macos")]
    fn process(kind: &str, args: &[String]) -> Result<Self> {
        use anyhow::Context;
        let mut child = crate::native::spawn(kind, args)?;
        let rd = child.stdout.take().context("panel stdout")?;
        let wr = child.stdin.take().context("panel stdin")?;
        tracing::debug!(kind, "panel opened as a helper process");
        Ok(Panel::Process {
            _child: child,
            rd,
            wr,
        })
    }

    #[cfg(not(target_os = "macos"))]
    fn process(_kind: &str, _args: &[String]) -> Result<Self> {
        bail!("native windows require macOS")
    }

    /// Send one command to the window.
    pub async fn send(&mut self, value: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        match self {
            #[cfg(target_os = "macos")]
            Panel::Process { wr, .. } => {
                crate::native::write(wr, crate::native::EVENT, &bytes).await
            }
            Panel::Viewer { id, host, .. } => {
                if host
                    .out
                    .send((*id, crate::native::EVENT, bytes))
                    .await
                    .is_err()
                {
                    bail!("the viewer is gone");
                }
                Ok(())
            }
        }
    }

    /// The window's next event. An error once it (or the viewer) is gone.
    pub async fn recv(&mut self) -> Result<Value> {
        match self {
            #[cfg(target_os = "macos")]
            Panel::Process { rd, .. } => {
                let (kind, bytes) = crate::native::read(rd).await?;
                anyhow::ensure!(kind == crate::native::EVENT, "unexpected window packet");
                Ok(serde_json::from_slice(&bytes)?)
            }
            Panel::Viewer { rx, .. } => loop {
                match rx.recv().await {
                    None => bail!("the viewer closed"),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(ev)) => match classify(ev) {
                        Routed::Keep(ev) => return Ok(ev),
                        Routed::Skip => continue,
                        Routed::Gone => bail!("Gliff quit"),
                    },
                }
            },
        }
    }
}

impl Drop for Panel {
    /// Close the window: a helper process is killed (`kill_on_drop`); a
    /// window in the viewer is told to close and its route is dropped.
    fn drop(&mut self) {
        if let Panel::Viewer { id, host, .. } = self {
            host.routes.borrow_mut().remove(id);
            let Ok(bytes) = serde_json::to_vec(&json!({"type": "close_panel"})) else {
                return;
            };
            let msg = (*id, crate::native::EVENT, bytes);
            if let Err(mpsc::error::TrySendError::Full(msg)) = host.out.try_send(msg) {
                //= The queue is busy with frames: send it as soon as it can.
                let out = host.out.clone();
                tokio::task::spawn_local(async move {
                    let _ = out.send(msg).await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> (ViewerHost, mpsc::Receiver<(u32, u8, Vec<u8>)>) {
        let (out, out_rx) = mpsc::channel(8);
        let host = ViewerHost {
            out,
            routes: Default::default(),
            next: Rc::new(Cell::new(3)),
        };
        (host, out_rx)
    }

    fn sent(rx: &mut mpsc::Receiver<(u32, u8, Vec<u8>)>) -> (u32, Value) {
        let (id, kind, bytes) = rx.try_recv().expect("a packet");
        assert_eq!(kind, crate::native::EVENT);
        (id, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn panels_ignore_layouts_and_end_on_quit() {
        assert_eq!(classify(json!({"type": "keyboard_layout"})), Routed::Skip);
        assert_eq!(classify(json!({"type": "bye"})), Routed::Gone);
        let ev = json!({"type": "connect", "window": 3});
        assert_eq!(classify(ev.clone()), Routed::Keep(ev));
    }

    #[tokio::test]
    async fn a_panel_in_the_viewer_takes_the_next_window_id() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (host, mut out) = host();
                set_viewer(Some(host.clone()));
                let mut panel = Panel::open("choose", &["private".into()]).await.unwrap();
                let (id, open) = sent(&mut out);
                assert_eq!(id, 3);
                assert_eq!(open, open_command("choose", &["private".into()]));
                assert_eq!(host.next.get(), 4, "the id is used up");
                assert!(host.routes.borrow().contains_key(&3));

                panel
                    .send(&json!({"type": "machine_settings"}))
                    .await
                    .unwrap();
                assert_eq!(sent(&mut out), (3, json!({"type": "machine_settings"})));

                let route = host.routes.borrow().get(&3).unwrap().clone();
                route.send(Ok(json!({"type": "keyboard_layout"}))).unwrap();
                route
                    .send(Ok(json!({"type": "cancel", "window": 3})))
                    .unwrap();
                assert_eq!(
                    panel.recv().await.unwrap()["type"],
                    "cancel",
                    "layouts skipped"
                );
                route.send(Ok(json!({"type": "bye"}))).unwrap();
                assert!(panel.recv().await.is_err(), "quitting ends the panel");

                drop(panel);
                assert!(!host.routes.borrow().contains_key(&3), "route dropped");
                assert_eq!(sent(&mut out), (3, json!({"type": "close_panel"})));
                set_viewer(None);
            })
            .await;
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn without_a_viewer_off_macos_there_is_no_window() {
        set_viewer(None);
        assert!(Panel::open("settings", &[]).await.is_err());
    }
}
