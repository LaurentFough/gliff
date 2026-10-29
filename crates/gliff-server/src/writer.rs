//! A dedicated task for the socket write half, so the session loop never
//! waits on the network and keeps handling input while a write is blocked.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gliff_proto::ServerMsg;
use gliff_transport::Framed;
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

/// Sound packets that may wait for the socket: half a second.
const MAX_QUEUED_AUDIO: usize = 50;
/// Maximum audio messages allowed to overtake one waiting video frame.
/// This guarantees progress even for peers without heartbeat support.
const MAX_AUDIO_OVERTAKES: usize = 50;

/// Messages waiting for the writer task. Bounded by coalescing: stale
/// entries a newer one supersedes are dropped rather than queued behind it.
#[derive(Default)]
struct Queue {
    packets: VecDeque<(ServerMsg, Vec<Vec<u8>>)>,
    audio_overtakes: usize,
}

impl Queue {
    /// True when no video frame is queued. The session encodes at most one
    /// frame ahead of the writer, so this gates the encoder.
    fn video_ready(&self) -> bool {
        !self
            .packets
            .iter()
            .any(|(msg, _)| matches!(msg, ServerMsg::VideoFrame { .. }))
    }

    fn push(&mut self, msg: ServerMsg, payloads: Vec<Vec<u8>>) {
        match &msg {
            ServerMsg::VideoFrame { .. } => {
                debug_assert!(self.video_ready());
                self.audio_overtakes = 0;
            }
            ServerMsg::StreamConfig { .. } => {
                let after_video = self
                    .packets
                    .iter()
                    .rposition(|(msg, _)| matches!(msg, ServerMsg::VideoFrame { .. }))
                    .map_or(0, |i| i + 1);
                let mut index = 0;
                self.packets.retain(|(msg, _)| {
                    let keep =
                        index < after_video || !matches!(msg, ServerMsg::StreamConfig { .. });
                    index += 1;
                    keep
                });
            }
            ServerMsg::CursorShape { .. } => self.packets.retain(|(msg, _)| {
                !matches!(
                    msg,
                    ServerMsg::CursorShape { .. } | ServerMsg::CursorPos { .. }
                )
            }),
            ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. } | ServerMsg::Ping { .. } => {
                self.packets.retain(|(queued, _)| {
                    std::mem::discriminant(queued) != std::mem::discriminant(&msg)
                });
            }
            // Clipboard transfers are ordered and reliable: never coalesce.
            // The transfer engine's ack window bounds how many can queue.
            ServerMsg::ClipboardOffer { .. }
            | ServerMsg::ClipboardRequest { .. }
            | ServerMsg::ClipboardData { .. }
            | ServerMsg::ClipboardAck { .. }
            | ServerMsg::ClipboardAbort { .. } => {}
            // Sound normally overtakes a waiting video frame, preserving
            // audio order. Pending Pings/Pongs limit that priority so they
            // cannot starve; cursor and clipboard updates are not barriers.
            // A per-video budget also guarantees progress without Pings.
            // Past a bound the oldest frame goes, and a restart replaces
            // the old stream so at most one start waits.
            ServerMsg::AudioStart { .. } | ServerMsg::AudioFrame { .. } => {
                let audio = |m: &ServerMsg| {
                    matches!(
                        m,
                        ServerMsg::AudioStart { .. } | ServerMsg::AudioFrame { .. }
                    )
                };
                if matches!(msg, ServerMsg::AudioStart { .. }) {
                    self.packets.retain(|(m, _)| !audio(m));
                } else if self.packets.iter().filter(|(m, _)| audio(m)).count() >= MAX_QUEUED_AUDIO
                {
                    if let Some(i) = self
                        .packets
                        .iter()
                        .position(|(m, _)| matches!(m, ServerMsg::AudioFrame { .. }))
                    {
                        self.packets.remove(i);
                    }
                }
                // Preserve heartbeat progress and existing audio order
                // before choosing a waiting video to overtake.
                let after_control = self
                    .packets
                    .iter()
                    .rposition(|(m, _)| {
                        matches!(m, ServerMsg::Ping { .. } | ServerMsg::Pong { .. })
                    })
                    .map_or(0, |index| index + 1);
                let after_audio = self
                    .packets
                    .iter()
                    .rposition(|(m, _)| audio(m))
                    .map_or(0, |index| index + 1);
                let first = after_control.max(after_audio);
                let at = if self.audio_overtakes < MAX_AUDIO_OVERTAKES {
                    self.packets
                        .iter()
                        .enumerate()
                        .skip(first)
                        .find(|(_, (m, _))| matches!(m, ServerMsg::VideoFrame { .. }))
                        .map_or(self.packets.len(), |(index, _)| index)
                } else {
                    self.packets.len()
                };
                if at < self.packets.len() {
                    self.audio_overtakes += 1;
                }
                self.packets.insert(at, (msg, payloads));
                return;
            }
            _ => unreachable!("unsupported queued server message"),
        }
        self.packets.push_back((msg, payloads));
    }
}

/// Owns the write half of the connection on its own task. `send` only
/// enqueues, so callers never block; write outcomes come back on the report
/// channel, where an `Err` means the connection is gone.
pub struct Writer {
    queue: Rc<RefCell<Queue>>,
    notify: Rc<Notify>,
    task: JoinHandle<()>,
}

impl Writer {
    pub fn spawn<W: AsyncWrite + Unpin + 'static>(
        mut framed: Framed<W>,
    ) -> (Self, mpsc::Receiver<Result<Duration>>) {
        let queue = Rc::new(RefCell::new(Queue::default()));
        let notify = Rc::new(Notify::new());
        let (report_tx, report_rx) = mpsc::channel(1);
        let task = {
            let queue = queue.clone();
            let notify = notify.clone();
            tokio::task::spawn_local(async move {
                loop {
                    let packet = queue.borrow_mut().packets.pop_front();
                    let Some((msg, payloads)) = packet else {
                        notify.notified().await;
                        continue;
                    };
                    let parts: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
                    let begin = Instant::now();
                    let result = framed.write_msg_with_payloads(&msg, &parts).await;
                    let elapsed = begin.elapsed();
                    tracing::debug!(
                        write_ms = elapsed.as_secs_f64() * 1000.0,
                        "output write completed"
                    );
                    let failed = result.is_err();
                    let report = result.map(|_| elapsed).map_err(Into::into);
                    if report_tx.send(report).await.is_err() || failed {
                        break;
                    }
                }
            })
        };
        (
            Self {
                queue,
                notify,
                task,
            },
            report_rx,
        )
    }

    /// Enqueue a message without blocking.
    pub fn send(&self, msg: ServerMsg, payloads: Vec<Vec<u8>>) {
        self.queue.borrow_mut().push(msg, payloads);
        self.notify.notify_one();
    }

    /// True when no video frame is waiting to be written.
    pub fn video_ready(&self) -> bool {
        self.queue.borrow().video_ready()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_proto::{ChromaMode, Codec, VideoPipeline};
    use tokio::task::LocalSet;

    fn video(id: u64) -> (ServerMsg, Vec<Vec<u8>>) {
        (
            ServerMsg::VideoFrame {
                frame_id: id,
                pts_us: 0,
                keyframe: id == 1,
                damage: Vec::new(),
                data_len: 64,
                aux_len: 0,
            },
            vec![vec![42; 64]],
        )
    }

    fn config(width: u32) -> (ServerMsg, Vec<Vec<u8>>) {
        (
            ServerMsg::StreamConfig {
                codec: Codec::H264,
                chroma: ChromaMode::Single420,
                pipeline: Some(VideoPipeline::Gpu),
                width,
                height: 480,
                scale_milli: 1000,
                extradata: Vec::new(),
                aux_extradata: None,
                view_width: width,
                view_height: 480,
                fps_cap: 60,
            },
            Vec::new(),
        )
    }

    #[test]
    fn coalescing_stays_bounded_and_keeps_video_config_order() {
        let mut queue = Queue::default();
        let (msg, payloads) = config(640);
        queue.push(msg, payloads);
        let (msg, payloads) = video(1);
        queue.push(msg, payloads);
        for id in 1..1000 {
            let (msg, payloads) = config(800 + id);
            queue.push(msg, payloads);
            queue.push(
                ServerMsg::CursorShape {
                    id,
                    width: 1,
                    height: 1,
                    hot_x: 0,
                    hot_y: 0,
                    argb_len: 4,
                },
                vec![vec![0; 4]],
            );
            queue.push(
                ServerMsg::CursorPos {
                    x: 0.0,
                    y: 0.0,
                    shape_id: id,
                    visible: true,
                },
                Vec::new(),
            );
            queue.push(
                ServerMsg::Pong {
                    t: id as u64,
                    server_now_ms: 0,
                },
                Vec::new(),
            );
            assert_eq!(queue.packets.len(), 6);
        }
        assert!(matches!(
            queue.packets[0].0,
            ServerMsg::StreamConfig { width: 640, .. }
        ));
        assert!(matches!(
            queue.packets[1].0,
            ServerMsg::VideoFrame { frame_id: 1, .. }
        ));
        assert!(matches!(
            queue.packets[2].0,
            ServerMsg::StreamConfig { width: 1799, .. }
        ));
        assert!(!queue.video_ready());
    }

    #[test]
    fn clipboard_messages_queue_in_order_without_coalescing() {
        let mut queue = Queue::default();
        for id in 0..3 {
            queue.push(
                ServerMsg::ClipboardData {
                    id,
                    offset: 0,
                    data_len: 1,
                    done: false,
                },
                vec![vec![0]],
            );
        }
        queue.push(ServerMsg::ClipboardAck { id: 9, received: 1 }, Vec::new());
        assert_eq!(queue.packets.len(), 4);
        assert!(matches!(
            queue.packets[0].0,
            ServerMsg::ClipboardData { id: 0, .. }
        ));
    }

    #[test]
    fn continuous_audio_cannot_starve_control_messages_or_reorder_starts() {
        let mut queue = Queue::default();
        let (msg, payloads) = video(1);
        queue.push(msg, payloads);
        queue.push(
            ServerMsg::Pong {
                t: 1,
                server_now_ms: 2,
            },
            Vec::new(),
        );
        queue.push(
            ServerMsg::AudioStart {
                codec: gliff_proto::AudioCodec::Opus,
                sample_rate: 48000,
                channels: 2,
            },
            Vec::new(),
        );
        for seq in 0..100 {
            queue.push(ServerMsg::AudioFrame { seq, data_len: 1 }, vec![vec![0]]);
        }
        assert!(matches!(queue.packets[0].0, ServerMsg::VideoFrame { .. }));
        assert!(matches!(queue.packets[1].0, ServerMsg::Pong { .. }));
        assert!(matches!(queue.packets[2].0, ServerMsg::AudioStart { .. }));
        let frames: Vec<_> = queue
            .packets
            .iter()
            .filter_map(|(msg, _)| match msg {
                ServerMsg::AudioFrame { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect();
        assert!(frames.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn cursor_updates_do_not_defeat_audio_priority_but_pings_get_progress() {
        let mut queue = Queue::default();
        let (msg, payloads) = video(1);
        queue.push(msg, payloads);
        queue.push(
            ServerMsg::CursorPos {
                x: 1.0,
                y: 2.0,
                shape_id: 0,
                visible: true,
            },
            Vec::new(),
        );
        queue.push(
            ServerMsg::AudioStart {
                codec: gliff_proto::AudioCodec::Opus,
                sample_rate: 48000,
                channels: 2,
            },
            Vec::new(),
        );
        queue.push(
            ServerMsg::AudioFrame {
                seq: 0,
                data_len: 1,
            },
            vec![vec![0]],
        );
        assert!(matches!(queue.packets[0].0, ServerMsg::AudioStart { .. }));
        assert!(matches!(
            queue.packets[1].0,
            ServerMsg::AudioFrame { seq: 0, .. }
        ));
        queue.push(ServerMsg::Ping { t: 10 }, Vec::new());
        for seq in 1..100 {
            queue.push(ServerMsg::AudioFrame { seq, data_len: 1 }, vec![vec![0]]);
        }
        let ping = queue
            .packets
            .iter()
            .position(|(msg, _)| matches!(msg, ServerMsg::Ping { .. }))
            .unwrap();
        let newest = queue
            .packets
            .iter()
            .position(|(msg, _)| matches!(msg, ServerMsg::AudioFrame { seq: 99, .. }))
            .unwrap();
        assert!(ping < newest);
    }

    #[test]
    fn audio_goes_ahead_of_a_waiting_frame_in_order_and_bounded() {
        let mut queue = Queue::default();
        let (msg, payloads) = video(1);
        queue.push(msg, payloads);
        queue.push(
            ServerMsg::AudioStart {
                codec: gliff_proto::AudioCodec::Opus,
                sample_rate: 48000,
                channels: 2,
            },
            Vec::new(),
        );
        for seq in 0..100 {
            queue.push(ServerMsg::AudioFrame { seq, data_len: 1 }, vec![vec![0]]);
        }
        assert_eq!(queue.packets.len(), MAX_QUEUED_AUDIO + 1);
        assert!(matches!(queue.packets[0].0, ServerMsg::AudioStart { .. }));
        assert!(matches!(
            queue.packets[1].0,
            ServerMsg::VideoFrame { frame_id: 1, .. }
        ));
        assert!(matches!(
            queue.packets[2].0,
            ServerMsg::AudioFrame { seq: 51, .. }
        ));
        assert!(matches!(
            queue.packets[MAX_QUEUED_AUDIO].0,
            ServerMsg::AudioFrame { seq: 99, .. }
        ));
    }

    #[test]
    fn slow_links_make_video_and_control_progress_without_heartbeats() {
        for restarting in [false, true] {
            let mut queue = Queue::default();
            let (msg, payloads) = video(1);
            queue.push(msg, payloads);
            queue.push(ServerMsg::ClipboardAck { id: 1, received: 0 }, Vec::new());
            let mut video_written = false;
            let mut control_written = false;
            for seq in 0..600 {
                if seq == 0 || restarting {
                    queue.push(
                        ServerMsg::AudioStart {
                            codec: gliff_proto::AudioCodec::Opus,
                            sample_rate: 48000,
                            channels: 2,
                        },
                        Vec::new(),
                    );
                }
                queue.push(ServerMsg::AudioFrame { seq, data_len: 1 }, vec![vec![0]]);
                // The link drains four times slower than audio arrives.
                if seq % 4 == 0 {
                    match queue.packets.pop_front().map(|(msg, _)| msg) {
                        Some(ServerMsg::VideoFrame { .. }) => video_written = true,
                        Some(ServerMsg::ClipboardAck { .. }) => control_written = true,
                        _ => {}
                    }
                }
                if video_written && control_written {
                    break;
                }
            }
            assert!(
                video_written && control_written,
                "starved with restarting={restarting}"
            );
        }
    }

    #[test]
    fn audio_restarts_replace_the_queued_stream() {
        let mut queue = Queue::default();
        let (msg, payloads) = video(1);
        queue.push(msg, payloads);
        for seq in 0..1000 {
            queue.push(
                ServerMsg::AudioStart {
                    codec: gliff_proto::AudioCodec::Opus,
                    sample_rate: 48000,
                    channels: 2,
                },
                Vec::new(),
            );
            queue.push(ServerMsg::AudioFrame { seq, data_len: 1 }, vec![vec![0]]);
            assert_eq!(queue.packets.len(), 3);
        }
        assert!(matches!(queue.packets[0].0, ServerMsg::VideoFrame { .. }));
        assert!(matches!(queue.packets[1].0, ServerMsg::AudioStart { .. }));
        assert!(matches!(
            queue.packets[2].0,
            ServerMsg::AudioFrame { seq: 999, .. }
        ));
    }

    #[tokio::test]
    async fn blocked_write_leaves_the_caller_free_and_preserves_frames() {
        LocalSet::new()
            .run_until(async {
                let (socket, peer) = tokio::io::duplex(1);
                let (writer, mut reports) = Writer::spawn(Framed::new(socket));
                let (msg, payloads) = video(1);
                writer.send(msg, payloads);
                tokio::task::yield_now().await;
                assert!(writer.video_ready());
                let (msg, payloads) = video(2);
                writer.send(msg, payloads);
                assert!(!writer.video_ready());
                let mut reader = Framed::new(peer);
                tokio::time::timeout(Duration::from_secs(2), async {
                    for expected in [1, 2] {
                        let msg: ServerMsg = reader.read_msg().await.unwrap();
                        assert!(
                            matches!(msg, ServerMsg::VideoFrame { frame_id, .. } if frame_id == expected)
                        );
                        assert_eq!(reader.read_payload(64).await.unwrap().as_ref(), &[42; 64]);
                        reports.recv().await.unwrap().unwrap();
                    }
                })
                .await
                .unwrap();
                assert!(writer.video_ready());
            })
            .await;
    }

    #[tokio::test]
    async fn write_failure_is_reported() {
        LocalSet::new()
            .run_until(async {
                let (socket, peer) = tokio::io::duplex(1);
                let (writer, mut reports) = Writer::spawn(Framed::new(socket));
                drop(peer);
                let (msg, payloads) = video(1);
                writer.send(msg, payloads);
                assert!(tokio::time::timeout(Duration::from_secs(1), reports.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn write_failure_behind_an_undrained_report_still_arrives() {
        LocalSet::new()
            .run_until(async {
                let (socket, peer) = tokio::io::duplex(1024 * 1024);
                let (writer, mut reports) = Writer::spawn(Framed::new(socket));
                let (msg, payloads) = video(1);
                writer.send(msg, payloads);
                tokio::task::yield_now().await;
                drop(peer);
                let (msg, payloads) = video(2);
                writer.send(msg, payloads);
                let first = tokio::time::timeout(Duration::from_secs(1), reports.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(first.is_ok());
                let second = tokio::time::timeout(Duration::from_secs(1), reports.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(second.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn dropping_the_writer_ends_the_task_while_a_write_is_blocked() {
        LocalSet::new()
            .run_until(async {
                let (socket, _peer) = tokio::io::duplex(1);
                let (writer, mut reports) = Writer::spawn(Framed::new(socket));
                let (msg, payloads) = video(1);
                writer.send(msg, payloads);
                tokio::task::yield_now().await;
                drop(writer);
                assert!(tokio::time::timeout(Duration::from_secs(1), reports.recv())
                    .await
                    .unwrap()
                    .is_none());
            })
            .await;
    }
}
