//! Bidirectional pipe abstraction for subsystem ↔ kernel communication.
//!
//! A pipe is two crossed mpsc channels. Each end can send and receive frames.
//! The [`PipeEnd`] supports two modes:
//!
//! - **Direct mode** (default): `recv()` reads from the raw channel. Used by
//!   subsystems that only handle requests and send responses.
//!
//! - **Dispatched mode**: Activated on the first `caller()` call. A background
//!   dispatcher task routes incoming frames by `parent_id` to pending
//!   [`CallStream`] receivers. Unmatched frames go to a default channel that
//!   `recv()` reads from. This is the "lazy dispatcher" pattern from Prior.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::PipeError;
use crate::frame::Frame;

const DEFAULT_CAPACITY: usize = 256;

/// Maps request ID → the unbounded sender for that request's `CallStream`.
///
/// Shared between the `Caller` (which inserts entries before sending requests)
/// and the dispatcher task (which removes entries on terminal frames). Access
/// is serialized by a `Mutex` because both sides run concurrently.
type PendingMap = HashMap<Uuid, mpsc::UnboundedSender<Frame>>;

/// Create a bidirectional pipe with the given channel capacity.
///
/// Returns two ends: each can send to and receive from the other.
#[must_use]
pub fn pipe(capacity: usize) -> (PipeEnd, PipeEnd) {
    let (a_tx, a_rx) = mpsc::channel(capacity);
    let (b_tx, b_rx) = mpsc::channel(capacity);

    let end_a = PipeEnd {
        tx: b_tx,
        state: PipeState::Direct(a_rx),
        pending: Arc::new(Mutex::new(HashMap::new())),
    };
    let end_b = PipeEnd {
        tx: a_tx,
        state: PipeState::Direct(b_rx),
        pending: Arc::new(Mutex::new(HashMap::new())),
    };
    (end_a, end_b)
}

/// Create a bidirectional pipe with the default capacity.
#[must_use]
pub fn pipe_default() -> (PipeEnd, PipeEnd) {
    pipe(DEFAULT_CAPACITY)
}

enum PipeState {
    /// Raw channel access — no dispatcher task running.
    Direct(mpsc::Receiver<Frame>),
    /// Dispatcher task running; `recv()` reads from this channel.
    Dispatched(mpsc::UnboundedReceiver<Frame>),
}

/// One end of a bidirectional pipe.
///
/// Subsystems interact with the kernel through a `PipeEnd`: inbound request
/// frames arrive via `recv()`, and response frames are sent back via `sender()`.
///
/// If a subsystem needs to call *other* subsystems through the kernel, it
/// calls `caller()` to obtain a [`Caller`]. This activates the lazy dispatcher,
/// which routes incoming responses back to the corresponding [`CallStream`]
/// rather than surfacing them via `recv()`.
///
/// # Invariant
///
/// Each `PipeEnd` is owned by exactly one subsystem task. It is not `Clone`
/// because sharing a single receive end between tasks would create a race for
/// inbound frames.
pub struct PipeEnd {
    /// Sends frames to the *other* end of the pipe (toward the kernel or subsystem).
    tx: mpsc::Sender<Frame>,
    state: PipeState,
    /// Shared with any `Caller` instances created from this end. The dispatcher
    /// uses this to route response frames to the correct `CallStream`.
    pending: Arc<Mutex<PendingMap>>,
}

impl PipeEnd {
    /// Receive the next inbound frame (request or unmatched response).
    pub async fn recv(&mut self) -> Option<Frame> {
        match &mut self.state {
            PipeState::Direct(rx) => rx.recv().await,
            PipeState::Dispatched(rx) => rx.recv().await,
        }
    }

    /// Get a cloneable sender for sending frames back through the pipe.
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<Frame> {
        self.tx.clone()
    }

    /// Get a [`Caller`] for making outbound requests through this pipe.
    ///
    /// On the first call, spawns a background dispatcher task that routes
    /// incoming responses by `parent_id` to pending [`CallStream`] receivers.
    pub fn caller(&mut self) -> Caller {
        self.ensure_dispatcher();
        Caller {
            tx: self.tx.clone(),
            pending: Arc::clone(&self.pending),
        }
    }

    /// Transition from direct to dispatched mode on the first `caller()` call.
    ///
    /// PHASE 1: IDEMPOTENCY CHECK
    /// Return immediately if already dispatched. `caller()` may be called
    /// multiple times; the dispatcher should only be spawned once.
    ///
    /// PHASE 2: STATE HANDOFF
    /// Replace the `Direct(rx)` state with `Dispatched(default_rx)`. The raw
    /// `Receiver` is handed off to the dispatcher task. After this point,
    /// `recv()` reads from `default_rx` (the unmatched-frames channel) instead
    /// of the raw channel.
    ///
    /// PHASE 3: SPAWN DISPATCHER
    /// The dispatcher task owns the raw receiver and the pending map. It routes
    /// matched frames to `CallStream`s and unmatched frames to `default_tx`.
    fn ensure_dispatcher(&mut self) {
        if matches!(self.state, PipeState::Dispatched(_)) {
            return;
        }

        let (default_tx, default_rx) = mpsc::unbounded_channel();
        let old_state = std::mem::replace(&mut self.state, PipeState::Dispatched(default_rx));

        let PipeState::Direct(raw_rx) = old_state else {
            // NOTE: This branch is unreachable given the check above, but the
            // compiler can't prove it. Return cleanly rather than panicking.
            return;
        };

        let pending = Arc::clone(&self.pending);
        tokio::spawn(run_dispatcher(raw_rx, default_tx, pending));
    }
}

/// Cloneable handle for making outbound requests and receiving correlated responses.
///
/// Obtained from [`PipeEnd::caller`]. Multiple `Caller` instances can be created
/// from the same `PipeEnd` and cloned freely — they all share the same underlying
/// channel and pending map. Each clone can make independent concurrent calls.
///
/// # Protocol
///
/// `call()` inserts an entry in the `PendingMap` before sending the request.
/// The dispatcher task (spawned by `ensure_dispatcher`) routes incoming frames
/// with matching `parent_id` to the registered `CallStream`. On terminal frames
/// or `CallStream` drop, the entry is removed.
#[derive(Clone)]
pub struct Caller {
    tx: mpsc::Sender<Frame>,
    /// Shared with the dispatcher task. Entries are inserted by `call()` and
    /// removed by the dispatcher on terminal frames or by `CallStream::drop`.
    pub(crate) pending: Arc<Mutex<PendingMap>>,
}

impl Caller {
    /// Send a request and receive a stream of correlated responses.
    ///
    /// Inserts a pending entry before sending so the dispatcher can route
    /// response frames correctly even if they arrive before `call` returns.
    /// If the send fails, the pending entry is cleaned up to avoid a leak.
    pub async fn call(&self, request: Frame) -> Result<CallStream, PipeError> {
        let id = request.id;
        let (stream_tx, stream_rx) = mpsc::unbounded_channel();

        // WHY: Insert before send, not after. If the response arrives before
        // the send returns, the dispatcher would find no pending entry and send
        // the frame to the default (unmatched) channel instead.
        {
            let mut guard = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.insert(id, stream_tx);
        }

        if self.tx.send(request).await.is_err() {
            let mut guard = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove(&id);
            return Err(PipeError::SendFailed);
        }

        Ok(CallStream {
            rx: stream_rx,
            id,
            pending: Arc::clone(&self.pending),
        })
    }

    /// Send a frame without expecting a response (fire-and-forget).
    pub async fn send(&self, frame: Frame) -> Result<(), PipeError> {
        self.tx.send(frame).await.map_err(|_| PipeError::SendFailed)
    }
}

/// An owned stream of response frames correlated to a single request.
///
/// Returned by [`Caller::call`]. Receives only frames whose `parent_id` matches
/// the originating request's `id`, allowing multiple concurrent calls over the
/// same pipe without interleaving.
///
/// # Cleanup on Drop
///
/// Dropping a `CallStream` before it reaches a terminal frame removes its entry
/// from the pending map. This prevents the dispatcher from accumulating entries
/// for abandoned calls. The channel receiver is dropped first, so the dispatcher
/// will see a send error and also clean up its reference.
pub struct CallStream {
    rx: mpsc::UnboundedReceiver<Frame>,
    /// The request ID this stream is correlated to, used for pending map cleanup.
    id: Uuid,
    pending: Arc<Mutex<PendingMap>>,
}

impl CallStream {
    /// Receive the next response frame.
    pub async fn recv(&mut self) -> Option<Frame> {
        self.rx.recv().await
    }

    /// Collect all frames until a terminal status, then return them.
    pub async fn collect(mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.rx.recv().await {
            let terminal = frame.status.is_terminal();
            frames.push(frame);
            if terminal {
                break;
            }
        }
        frames
    }
}

impl Drop for CallStream {
    /// Remove the pending map entry when the stream is dropped.
    ///
    /// WHY: Without this, dropping a `CallStream` mid-stream would leave a
    /// dead `UnboundedSender` in the pending map. The dispatcher would keep
    /// sending to it (succeeding, since unbounded channels don't fail on send
    /// to a dead receiver) until a terminal frame or until the pending map is
    /// leaked forever if the subsystem never sends a terminal frame.
    fn drop(&mut self) {
        let mut guard = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&self.id);
    }
}

/// Background dispatcher task: routes incoming frames to `CallStream`s by `parent_id`.
///
/// This task embodies the "lazy dispatcher" pattern: it is only spawned when a
/// subsystem first calls `PipeEnd::caller()`, so subsystems that never make
/// outbound calls pay zero overhead.
///
/// # Routing Logic
///
/// For each incoming frame:
/// - If `parent_id` matches a pending `CallStream`, forward to that stream.
/// - Otherwise, forward to `default_tx` so `PipeEnd::recv()` can surface it
///   (e.g., inbound requests arriving at the subsystem's pipe end).
///
/// # Cleanup
///
/// Pending entries are removed when a terminal frame is delivered, or when the
/// `CallStream` receiver has been dropped (send failure). This prevents the
/// pending map from growing unboundedly.
///
/// The task exits when `raw_rx` is closed (the kernel or `PipeEnd` was dropped).
/// At that point, `default_tx` is dropped, causing `PipeEnd::recv()` to return
/// `None` and the subsystem's receive loop to terminate naturally.
async fn run_dispatcher(
    mut raw_rx: mpsc::Receiver<Frame>,
    default_tx: mpsc::UnboundedSender<Frame>,
    pending: Arc<Mutex<PendingMap>>,
) {
    while let Some(frame) = raw_rx.recv().await {
        let parent_id = frame.parent_id;
        let is_terminal = frame.status.is_terminal();

        // Look up the CallStream for this frame's parent without holding the
        // lock during the send — lock durations should be as short as possible.
        let target = parent_id.and_then(|pid| {
            let guard = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.get(&pid).cloned()
        });

        let Some(stream_tx) = target else {
            // EDGE: No matching CallStream — route to default channel so the
            // PipeEnd's recv() sees it. Inbound requests arrive this way.
            if default_tx.send(frame).is_err() {
                break; // Default receiver dropped — pipe end is gone, exit task
            }
            continue;
        };

        let send_failed = stream_tx.send(frame).is_err();

        // Clean up the pending entry on terminal frames or send failures.
        // Terminal: no more frames will arrive for this stream.
        // Send failure: CallStream was dropped, so its receiver is gone.
        if is_terminal || send_failed {
            if let Some(pid) = parent_id {
                let mut guard = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.remove(&pid);
            }
        }
    }

    // If the raw pipe closes abruptly, drop all pending stream senders so
    // in-flight CallStreams wake up with `None` instead of hanging forever.
    let mut guard = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.clear();
}

#[cfg(test)]
#[path = "pipe_test.rs"]
mod tests;
