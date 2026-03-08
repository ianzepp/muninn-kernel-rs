//! `FrameSender` — helper for sending response frames with common patterns.

use serde::Serialize;
use tokio::sync::mpsc;

use crate::error::PipeError;
use crate::frame::{ErrorCode, Frame, to_data};

/// Helper for sending response frames through a channel.
///
/// Provides convenience methods for common response patterns:
/// single item + done, multiple items + done, or error.
#[derive(Clone)]
pub struct FrameSender {
    tx: mpsc::Sender<Frame>,
}

impl FrameSender {
    /// Create a new sender wrapping an mpsc channel.
    #[must_use] 
    pub fn new(tx: mpsc::Sender<Frame>) -> Self {
        Self { tx }
    }

    /// Send a raw frame.
    pub async fn send(&self, frame: Frame) -> Result<(), PipeError> {
        self.tx.send(frame).await.map_err(|_| PipeError::SendFailed)
    }

    /// Send an item frame with serializable data.
    pub async fn send_item<T: Serialize>(&self, req: &Frame, value: &T) -> Result<(), PipeError> {
        let data = to_data(value)?;
        self.send(req.item(data)).await
    }

    /// Send a done frame.
    pub async fn send_done(&self, req: &Frame) -> Result<(), PipeError> {
        self.send(req.done()).await
    }

    /// Send an error frame with a message.
    pub async fn send_error(
        &self,
        req: &Frame,
        message: impl Into<String>,
    ) -> Result<(), PipeError> {
        self.send(req.error(message)).await
    }

    /// Send an error frame from an [`ErrorCode`] implementor.
    pub async fn send_error_from(
        &self,
        req: &Frame,
        err: &impl ErrorCode,
    ) -> Result<(), PipeError> {
        self.send(req.error_from(err)).await
    }

    /// Send a single item then done, or an error on failure.
    pub async fn finish_item<T: Serialize, E: ErrorCode>(
        &self,
        req: &Frame,
        result: Result<T, E>,
    ) -> Result<(), PipeError> {
        match result {
            Ok(value) => {
                self.send_item(req, &value).await?;
                self.send_done(req).await
            }
            Err(e) => self.send_error_from(req, &e).await,
        }
    }

    /// Send multiple items then done, or an error on failure.
    pub async fn finish_items<T: Serialize, E: ErrorCode>(
        &self,
        req: &Frame,
        result: Result<Vec<T>, E>,
    ) -> Result<(), PipeError> {
        match result {
            Ok(items) => {
                for item in &items {
                    self.send_item(req, item).await?;
                }
                self.send_done(req).await
            }
            Err(e) => self.send_error_from(req, &e).await,
        }
    }

    /// Send done or error based on a unit result.
    pub async fn finish<E: ErrorCode>(
        &self,
        req: &Frame,
        result: Result<(), E>,
    ) -> Result<(), PipeError> {
        match result {
            Ok(()) => self.send_done(req).await,
            Err(e) => self.send_error_from(req, &e).await,
        }
    }
}

#[cfg(test)]
#[path = "sender_test.rs"]
mod tests;
