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
pub struct PipeEnd {
    tx: mpsc::Sender<Frame>,
    state: PipeState,
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

    /// Transition from direct to dispatched mode if not already.
    fn ensure_dispatcher(&mut self) {
        if matches!(self.state, PipeState::Dispatched(_)) {
            return;
        }

        let (default_tx, default_rx) = mpsc::unbounded_channel();
        let old_state = std::mem::replace(&mut self.state, PipeState::Dispatched(default_rx));

        let PipeState::Direct(raw_rx) = old_state else {
            return;
        };

        let pending = Arc::clone(&self.pending);
        tokio::spawn(run_dispatcher(raw_rx, default_tx, pending));
    }
}

/// Cloneable handle for making outbound requests and receiving correlated responses.
#[derive(Clone)]
pub struct Caller {
    tx: mpsc::Sender<Frame>,
    pending: Arc<Mutex<PendingMap>>,
}

impl Caller {
    /// Send a request and receive a stream of correlated responses.
    pub async fn call(&self, request: Frame) -> Result<CallStream, PipeError> {
        let id = request.id;
        let (stream_tx, stream_rx) = mpsc::unbounded_channel();

        {
            let mut guard = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.insert(id, stream_tx);
        }

        if self.tx.send(request).await.is_err() {
            let mut guard = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
pub struct CallStream {
    rx: mpsc::UnboundedReceiver<Frame>,
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
    fn drop(&mut self) {
        let mut guard = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&self.id);
    }
}

/// Background dispatcher task: routes incoming frames by `parent_id` to pending
/// `CallStream` receivers. Unmatched frames go to the default channel.
async fn run_dispatcher(
    mut raw_rx: mpsc::Receiver<Frame>,
    default_tx: mpsc::UnboundedSender<Frame>,
    pending: Arc<Mutex<PendingMap>>,
) {
    while let Some(frame) = raw_rx.recv().await {
        let parent_id = frame.parent_id;
        let is_terminal = frame.status.is_terminal();

        let target = parent_id.and_then(|pid| {
            let guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.get(&pid).cloned()
        });

        let Some(stream_tx) = target else {
            if default_tx.send(frame).is_err() {
                break;
            }
            continue;
        };

        let send_failed = stream_tx.send(frame).is_err();

        if is_terminal || send_failed {
            if let Some(pid) = parent_id {
                let mut guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.remove(&pid);
            }
        }
    }
}

#[cfg(test)]
#[path = "pipe_test.rs"]
mod tests;
