//! One connection's outgoing queue, bounded in bytes and messages.
//!
//! The hub pushes; the connection's writer drains everything queued into
//! one frame per wake. Consecutive text deltas of the same item merge while
//! they wait (a slow reader gets fewer, larger deltas). When state messages
//! would push the queue past its bound, the state backlog is dropped
//! instead ([`Push::Overflow`]): the hub then tells the client to expect
//! fresh snapshots. Control and terminal messages are never dropped;
//! terminal readers wait for room instead ([`Outbox::wait_room`]).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use blongo_protocol::wire::{Payload, Sequenced, ServerMsg};
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug)]
pub struct OutboxLimits {
    pub max_bytes: usize,
    pub max_msgs: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    Queued,
    /// The state backlog was dropped; the client must resnapshot.
    Overflow,
    Closed,
}

struct State {
    queue: VecDeque<(ServerMsg, usize)>,
    bytes: usize,
    /// Bytes of queued snapshots: they replace the client's state, so they
    /// do not count against the bound (a thread larger than the bound
    /// could otherwise never be sent).
    snapshot_bytes: usize,
    peak_bytes: usize,
}

fn is_snapshot(msg: &ServerMsg) -> bool {
    matches!(
        msg,
        ServerMsg::Seq(Sequenced {
            payload: Payload::Shell(_) | Payload::Thread(_),
            ..
        })
    )
}

pub struct Outbox {
    state: Mutex<State>,
    wake: Notify,
    room: Notify,
    closed: AtomicBool,
    closed_notify: Notify,
    limits: OutboxLimits,
}

fn size_of(msg: &ServerMsg) -> usize {
    match msg {
        ServerMsg::Seq(s) => s.payload.approx_size(),
        ServerMsg::TerminalOutput { data, .. } => 32 + data.len(),
        ServerMsg::Models { models, .. } => 64 + 64 * models.len(),
        _ => 128,
    }
}

impl Outbox {
    pub fn new(limits: OutboxLimits) -> Self {
        Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                bytes: 0,
                snapshot_bytes: 0,
                peak_bytes: 0,
            }),
            wake: Notify::new(),
            room: Notify::new(),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
            limits,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.notify_one();
        self.room.notify_waiters();
        self.closed_notify.notify_waiters();
    }

    /// Resolves once the outbox is closed.
    pub async fn closed(&self) {
        loop {
            let notified = self.closed_notify.notified();
            if self.is_closed() {
                return;
            }
            notified.await;
        }
    }

    pub fn bytes(&self) -> usize {
        self.state.lock().expect("outbox").bytes
    }

    /// Most bytes of state (snapshots aside) ever queued.
    pub fn peak_bytes(&self) -> usize {
        self.state.lock().expect("outbox").peak_bytes
    }

    pub fn len(&self) -> usize {
        self.state.lock().expect("outbox").queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Queue a message. State messages (`Seq`) may overflow the bound;
    /// everything else is always queued.
    pub fn push(&self, msg: ServerMsg) -> Push {
        if self.is_closed() {
            return Push::Closed;
        }
        let mut st = self.state.lock().expect("outbox");
        let size = size_of(&msg);
        // Merge a text delta into a waiting delta of the same item.
        if let ServerMsg::Seq(Sequenced {
            seq,
            payload: Payload::TextDelta { item_id, chunk, .. },
        }) = &msg
            && let Some((
                ServerMsg::Seq(Sequenced {
                    seq: last_seq,
                    payload:
                        Payload::TextDelta {
                            item_id: last_item,
                            chunk: last_chunk,
                            ..
                        },
                }),
                last_size,
            )) = st.queue.back_mut()
            && last_item == item_id
        {
            let mut merged = String::with_capacity(last_chunk.len() + chunk.len());
            merged.push_str(last_chunk);
            merged.push_str(chunk);
            *last_chunk = merged.into();
            *last_seq = *seq;
            *last_size += chunk.len();
            st.bytes += chunk.len();
        } else {
            if is_snapshot(&msg) {
                st.snapshot_bytes += size;
            }
            st.queue.push_back((msg, size));
            st.bytes += size;
        }
        let overflow = st.bytes - st.snapshot_bytes > self.limits.max_bytes
            || st.queue.len() > self.limits.max_msgs;
        if overflow {
            // Keep control and terminal messages; drop the state backlog
            // (snapshots included: fresh ones follow).
            st.queue.retain(|(m, _)| !matches!(m, ServerMsg::Seq(_)));
            st.bytes = st.queue.iter().map(|(_, s)| s).sum();
            st.snapshot_bytes = 0;
        }
        st.peak_bytes = st.peak_bytes.max(st.bytes - st.snapshot_bytes);
        drop(st);
        self.wake.notify_one();
        if overflow {
            Push::Overflow
        } else {
            Push::Queued
        }
    }

    /// Wait until the queue is below half its byte bound (terminal output
    /// pauses instead of being dropped). `false`: closed.
    pub async fn wait_room(&self) -> bool {
        loop {
            let room = self.room.notified();
            if self.is_closed() {
                return false;
            }
            if self.bytes() < self.limits.max_bytes / 2 {
                return true;
            }
            room.await;
        }
    }

    /// Blocking form of [`Self::wait_room`] for reader threads.
    pub fn wait_room_blocking(&self) -> bool {
        loop {
            if self.is_closed() {
                return false;
            }
            if self.bytes() < self.limits.max_bytes / 2 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Take up to `max_bytes` worth of messages (at least one), waiting
    /// for some. `None`: closed and nothing left to send.
    pub async fn drain(&self, max_bytes: usize) -> Option<Vec<ServerMsg>> {
        loop {
            let woken = self.wake.notified();
            {
                let mut st = self.state.lock().expect("outbox");
                if !st.queue.is_empty() {
                    let mut out = Vec::new();
                    let mut taken = 0;
                    let mut snapshots = 0;
                    while let Some((_, size)) = st.queue.front() {
                        if !out.is_empty() && taken + size > max_bytes {
                            break;
                        }
                        let (msg, size) = st.queue.pop_front().expect("front");
                        taken += size;
                        if is_snapshot(&msg) {
                            snapshots += size;
                        }
                        out.push(msg);
                    }
                    st.bytes -= taken;
                    st.snapshot_bytes -= snapshots;
                    if st.queue.is_empty() && st.queue.capacity() > 256 {
                        st.queue.shrink_to(64);
                    }
                    drop(st);
                    self.room.notify_waiters();
                    return Some(out);
                }
            }
            if self.is_closed() {
                return None;
            }
            woken.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use blongo_protocol::{ItemId, ThreadId};

    fn delta(seq: u64, item: ItemId, text: &str) -> ServerMsg {
        ServerMsg::Seq(Sequenced {
            seq,
            payload: Payload::TextDelta {
                thread_id: ThreadId(blongo_protocol::Uuid::nil()),
                item_id: item,
                chunk: text.into(),
            },
        })
    }

    #[tokio::test]
    async fn deltas_coalesce_and_keep_the_last_sequence() {
        let out = Outbox::new(OutboxLimits {
            max_bytes: 1 << 20,
            max_msgs: 100,
        });
        let (a, b) = (ItemId::new(), ItemId::new());
        out.push(delta(1, a, "he"));
        out.push(delta(2, a, "llo"));
        out.push(delta(3, b, "x"));
        out.push(delta(4, a, "!"));
        let msgs = out.drain(1 << 20).await.unwrap();
        let got: Vec<(u64, String)> = msgs
            .iter()
            .map(|m| match m {
                ServerMsg::Seq(Sequenced {
                    seq,
                    payload: Payload::TextDelta { chunk, .. },
                }) => (*seq, chunk.to_string()),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            got,
            vec![(2, "hello".into()), (3, "x".into()), (4, "!".into())]
        );
        assert_eq!(out.bytes(), 0);
    }

    #[tokio::test]
    async fn overflow_drops_state_but_keeps_control() {
        let out = Outbox::new(OutboxLimits {
            max_bytes: 2000,
            max_msgs: 1000,
        });
        out.push(ServerMsg::Pong { at: 1 });
        let mut overflowed = false;
        for i in 0..100 {
            if out.push(delta(i, ItemId::new(), &"x".repeat(100))) == Push::Overflow {
                overflowed = true;
                break;
            }
        }
        assert!(overflowed);
        assert!(out.bytes() <= 2000);
        let msgs = out.drain(1 << 20).await.unwrap();
        assert_eq!(msgs, vec![ServerMsg::Pong { at: 1 }]);
        assert!(out.peak_bytes() <= 2000 + 300);
    }

    #[tokio::test]
    async fn drain_waits_and_close_ends_it() {
        let out = Arc::new(Outbox::new(OutboxLimits {
            max_bytes: 1 << 20,
            max_msgs: 100,
        }));
        let o = out.clone();
        let task = tokio::spawn(async move { o.drain(1 << 20).await });
        tokio::task::yield_now().await;
        out.push(ServerMsg::Pong { at: 2 });
        assert_eq!(task.await.unwrap().unwrap().len(), 1);
        out.close();
        assert!(out.drain(1 << 20).await.is_none());
        assert_eq!(out.push(ServerMsg::Pong { at: 3 }), Push::Closed);
        out.closed().await;
    }
}
