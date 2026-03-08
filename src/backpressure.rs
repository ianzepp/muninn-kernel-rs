//! Watermark-based backpressure for subscriber delivery.
//!
//! A [`StreamController`] wraps an `mpsc::Sender<Frame>` and applies flow
//! control per response stream (keyed by `parent_id`). When the number of
//! in-flight frames for a stream exceeds `high_watermark`, the controller
//! switches from non-blocking `try_send` to blocking `send` with a stall
//! timeout. If the consumer doesn't drain within `stall_timeout`, the stream
//! is considered stalled.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::frame::Frame;

/// Default high watermark: pause producer when buffered frames exceed this.
const DEFAULT_HIGH_WATERMARK: usize = 1000;

/// Default low watermark: resume fast path when buffered frames drop below this.
const DEFAULT_LOW_WATERMARK: usize = 100;

/// Default stall timeout: cancel stream if consumer doesn't drain in time.
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for backpressure behavior.
#[derive(Clone, Debug)]
pub struct BackpressureConfig {
    /// Pause producer when buffered frames for a stream exceed this.
    pub high_watermark: usize,
    /// Resume fast path when buffered frames drop below this.
    pub low_watermark: usize,
    /// Cancel stream if a blocking send doesn't complete within this duration.
    pub stall_timeout: Duration,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        Self {
            high_watermark: DEFAULT_HIGH_WATERMARK,
            low_watermark: DEFAULT_LOW_WATERMARK,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        }
    }
}

/// Per-stream flow state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlowState {
    /// Fast path: use `try_send`.
    Flowing,
    /// Slow path: use send with timeout.
    Paused,
}

/// Tracks buffered frame counts per response stream (by `parent_id`).
type StreamCounts = HashMap<Uuid, usize>;

/// Result of a send attempt through the controller.
#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// Frame was delivered successfully.
    Delivered,
    /// Consumer is stalled beyond the timeout — stream should be cancelled.
    Stalled,
    /// Channel is closed — subscriber is gone.
    Closed,
}

/// Flow-controlled wrapper around an `mpsc::Sender<Frame>`.
///
/// Tracks per-stream buffer depth and applies backpressure when a consumer
/// falls behind. Used by the kernel to wrap subscriber channels.
#[derive(Clone)]
pub struct StreamController {
    tx: mpsc::Sender<Frame>,
    config: BackpressureConfig,
    counts: Arc<Mutex<StreamCounts>>,
    paused: Arc<Mutex<HashSet<Uuid>>>,
}

impl StreamController {
    /// Create a new stream controller wrapping the given sender.
    #[must_use]
    pub fn new(tx: mpsc::Sender<Frame>, config: BackpressureConfig) -> Self {
        Self {
            tx,
            config,
            counts: Arc::new(Mutex::new(HashMap::new())),
            paused: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Create a stream controller with default configuration.
    #[must_use]
    pub fn with_defaults(tx: mpsc::Sender<Frame>) -> Self {
        Self::new(tx, BackpressureConfig::default())
    }

    /// Send a frame with flow control.
    ///
    /// For non-terminal frames: tracks buffer depth per stream and applies
    /// backpressure when `high_watermark` is exceeded. For terminal frames:
    /// always attempts delivery and cleans up stream tracking.
    pub async fn send(&self, frame: &Frame) -> SendOutcome {
        let Some(stream_id) = frame.parent_id else {
            return self.try_deliver(frame).await;
        };

        let is_terminal = frame.status.is_terminal();

        if is_terminal {
            // Always try to deliver terminal frames, then clean up
            let outcome = self.try_deliver(frame).await;
            self.remove_stream(stream_id);
            self.resume_stream(stream_id);
            return outcome;
        }

        // Check flow state for this stream
        let state = self.flow_state(stream_id);

        let outcome = match state {
            FlowState::Flowing => self.try_deliver(frame).await,
            FlowState::Paused => self.blocking_deliver(frame).await,
        };

        if outcome == SendOutcome::Delivered {
            self.increment(stream_id);
            self.update_flow_state(stream_id);
        }

        outcome
    }

    /// Record that the consumer has processed frames (called externally or
    /// when the channel drains). For now, terminal frames reset the count.
    pub fn ack_stream(&self, stream_id: Uuid) {
        self.remove_stream(stream_id);
        self.resume_stream(stream_id);
    }

    /// Get the current buffered count for a stream.
    #[must_use]
    pub fn buffered(&self, stream_id: Uuid) -> usize {
        let guard = self.counts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(&stream_id).copied().unwrap_or(0)
    }

    /// Get a reference to the underlying sender (for compatibility).
    #[must_use]
    pub fn sender(&self) -> &mpsc::Sender<Frame> {
        &self.tx
    }

    pub(crate) fn ack_frame(&self, frame: &Frame) {
        let Some(stream_id) = frame.parent_id else {
            return;
        };

        if frame.status.is_terminal() {
            self.remove_stream(stream_id);
            self.resume_stream(stream_id);
            return;
        }

        self.decrement(stream_id);
        self.update_flow_state(stream_id);
    }

    // ── Internal ──

    fn flow_state(&self, stream_id: Uuid) -> FlowState {
        let count = self.buffered(stream_id);
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if paused.contains(&stream_id) {
            if count <= self.config.low_watermark {
                paused.remove(&stream_id);
                FlowState::Flowing
            } else {
                FlowState::Paused
            }
        } else if count >= self.config.high_watermark {
            paused.insert(stream_id);
            FlowState::Paused
        } else {
            FlowState::Flowing
        }
    }

    fn increment(&self, stream_id: Uuid) {
        let mut guard = self.counts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard.entry(stream_id).or_insert(0) += 1;
    }

    fn decrement(&self, stream_id: Uuid) {
        let mut guard = self.counts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(count) = guard.get_mut(&stream_id) else {
            return;
        };

        if *count <= 1 {
            guard.remove(&stream_id);
        } else {
            *count -= 1;
        }
    }

    fn remove_stream(&self, stream_id: Uuid) {
        let mut guard = self.counts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&stream_id);
    }

    fn update_flow_state(&self, stream_id: Uuid) {
        let count = self.buffered(stream_id);
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if paused.contains(&stream_id) {
            if count <= self.config.low_watermark {
                paused.remove(&stream_id);
            }
        } else if count >= self.config.high_watermark {
            paused.insert(stream_id);
        }
    }

    fn resume_stream(&self, stream_id: Uuid) {
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        paused.remove(&stream_id);
    }

    /// Fast path: non-blocking `try_send`.
    async fn try_deliver(&self, frame: &Frame) -> SendOutcome {
        match self.tx.try_send(frame.clone()) {
            Ok(()) => SendOutcome::Delivered,
            Err(mpsc::error::TrySendError::Full(_frame)) => {
                // Channel is full — fall back to blocking with timeout
                self.blocking_deliver(frame).await
            }
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        }
    }

    /// Slow path: blocking send with stall timeout.
    async fn blocking_deliver(&self, frame: &Frame) -> SendOutcome {
        match tokio::time::timeout(self.config.stall_timeout, self.tx.send(frame.clone())).await {
            Ok(Ok(())) => SendOutcome::Delivered,
            Ok(Err(_)) => SendOutcome::Closed,
            Err(_) => SendOutcome::Stalled,
        }
    }
}

/// Subscriber wrapper that acknowledges frames back to the stream controller.
pub struct Subscriber {
    rx: mpsc::Receiver<Frame>,
    controller: StreamController,
}

impl Subscriber {
    #[must_use]
    pub fn new(rx: mpsc::Receiver<Frame>, controller: StreamController) -> Self {
        Self { rx, controller }
    }

    pub async fn recv(&mut self) -> Option<Frame> {
        let frame = self.rx.recv().await?;
        self.controller.ack_frame(&frame);
        Some(frame)
    }
}

#[cfg(test)]
#[path = "backpressure_test.rs"]
mod tests;
