//! Bounded network writes independent of capture and input handling.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use gliff_proto::ServerMsg;
use gliff_transport::Framed;
use std::time::Duration;
use tokio::{io::AsyncWrite, sync::mpsc, task::JoinHandle};

pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Writer {
    outgoing: mpsc::Sender<(ServerMsg, Vec<Bytes>)>,
    task: JoinHandle<Result<()>>,
}

impl Writer {
    pub(crate) fn spawn<W: AsyncWrite + Unpin + 'static>(mut framed: Framed<W>) -> Self {
        let (outgoing, mut queue) = mpsc::channel::<(ServerMsg, Vec<Bytes>)>(16);
        let task = tokio::task::spawn_local(async move {
            while let Some((msg, payloads)) = queue.recv().await {
                let parts: Vec<&[u8]> = payloads.iter().map(Bytes::as_ref).collect();
                tokio::time::timeout(WRITE_TIMEOUT, framed.write_msg_with_payloads(&msg, &parts))
                    .await
                    .context("viewer stopped reading for 5 seconds")??;
            }
            Ok(())
        });
        Self { outgoing, task }
    }

    /// Never wait behind a stalled viewer, including for clipboard and pings.
    /// Exhausting the bounded queue disconnects rather than dropping packets.
    pub(crate) fn send(&self, msg: ServerMsg, payloads: Vec<Bytes>) -> Result<()> {
        self.outgoing
            .try_send((msg, payloads))
            .context("network output queue is full or closed")
    }

    /// Queue a packet that may be lost (sound: the client conceals a gap in
    /// `seq`). A full queue drops it, false; only a stopped writer is an error.
    pub(crate) fn offer(&self, msg: ServerMsg, payloads: Vec<Bytes>) -> Result<bool> {
        match self.outgoing.try_send((msg, payloads)) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("network writer stopped"),
        }
    }

    pub(crate) async fn stopped(&mut self) -> Result<()> {
        (&mut self.task).await??;
        bail!("network writer stopped")
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
