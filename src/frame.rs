//! Frame type — the universal in-memory message for all kernel communication.
//!
//! All kernel communication uses `Frame` structs moved through `mpsc` channels.
//! No serialization occurs inside the kernel: frames carry native Rust types and
//! are only converted to wire formats (protobuf, JSON) at I/O boundaries by gateway
//! code external to this crate.
//!
//! # Correlation
//!
//! Response frames are correlated to their request via `parent_id`. The router
//! uses `parent_id` to look up pending subscribers in its routing tables. Every
//! response builder method (`item`, `done`, `error`, etc.) sets `parent_id =
//! self.id`, so callers cannot accidentally break correlation.
//!
//! # Data vs Trace
//!
//! `data` carries the business payload. `trace` carries observability metadata
//! (room, span, timing). Keeping them separate prevents observability metadata
//! from polluting the application payload that subsystems deserialize into typed
//! structs. The router propagates `trace` through response frames automatically
//! (via the `response()` helper) so handlers never need to copy it manually.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Flat key-value payload carried by a frame.
pub type Data = HashMap<String, Value>;

/// Trait for subsystem errors that can be converted to structured error frames.
pub trait ErrorCode: std::fmt::Display {
    /// Machine-readable error code (e.g., `"E_NOT_FOUND"`).
    fn error_code(&self) -> &'static str;

    /// Whether the caller should retry this operation.
    fn retryable(&self) -> bool {
        false
    }
}

/// Lifecycle status of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Initiates a call.
    Request,
    /// Single streaming result (non-terminal).
    Item,
    /// Batch streaming result (non-terminal).
    Bulk,
    /// Successful terminal response.
    Done,
    /// Failed terminal response.
    Error,
    /// Abort an in-flight request.
    Cancel,
}

impl Status {
    /// Returns `true` if this status ends a response stream.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Error | Self::Cancel)
    }
}

/// A single message in the kernel's in-memory protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    /// Unique identifier for this frame.
    pub id: Uuid,
    /// Correlates response frames to the originating request.
    pub parent_id: Option<Uuid>,
    /// Milliseconds since the Unix epoch when the frame was created.
    pub created_ms: i64,
    /// Milliseconds from `created_ms` after which this frame should be considered expired.
    /// Zero means no expiration.
    pub expires_in: i64,
    /// Sender identity (user ID, subsystem label, actor name).
    pub from: Option<String>,
    /// Namespaced call name used for routing: `"prefix:verb"`.
    pub call: String,
    /// Lifecycle position.
    pub status: Status,
    /// Observability metadata (room, span, timing), separate from payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<Value>,
    /// Business payload.
    #[serde(default)]
    pub data: Data,
}

impl Frame {
    // ── Constructors ──

    /// Create a new request frame.
    #[must_use]
    pub fn request(call: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            parent_id: None,
            created_ms: now_millis(),
            expires_in: 0,
            from: None,
            call: call.into(),
            status: Status::Request,
            trace: None,
            data: HashMap::new(),
        }
    }

    /// Create a new request frame with data.
    #[must_use]
    pub fn request_with(call: impl Into<String>, data: Data) -> Self {
        let mut frame = Self::request(call);
        frame.data = data;
        frame
    }

    // ── Response builders (set parent_id = self.id) ──

    /// Build an item response correlated to this request.
    #[must_use]
    pub fn item(&self, data: Data) -> Self {
        self.response(Status::Item, data)
    }

    /// Build an item response from a serializable value.
    ///
    /// Serializes the value to a flat data map. If the value serializes to a
    /// JSON object, its fields become the data keys. Otherwise the value is
    /// stored under a `"value"` key.
    pub fn item_from<T: Serialize>(&self, value: &T) -> Result<Self, serde_json::Error> {
        Ok(self.response(Status::Item, to_data(value)?))
    }

    /// Build a bulk response correlated to this request.
    #[must_use]
    pub fn bulk(&self, data: Data) -> Self {
        self.response(Status::Bulk, data)
    }

    /// Build a bulk response from a serializable value.
    pub fn bulk_from<T: Serialize>(&self, value: &T) -> Result<Self, serde_json::Error> {
        Ok(self.response(Status::Bulk, to_data(value)?))
    }

    /// Build a done response correlated to this request.
    #[must_use]
    pub fn done(&self) -> Self {
        self.response(Status::Done, HashMap::new())
    }

    /// Build a done response with data correlated to this request.
    #[must_use]
    pub fn done_with(&self, data: Data) -> Self {
        self.response(Status::Done, data)
    }

    /// Build a done response from a serializable value.
    pub fn done_from<T: Serialize>(&self, value: &T) -> Result<Self, serde_json::Error> {
        Ok(self.response(Status::Done, to_data(value)?))
    }

    /// Build an error response correlated to this request.
    #[must_use]
    pub fn error(&self, message: impl Into<String>) -> Self {
        let mut data = HashMap::new();
        data.insert("code".into(), Value::String("E_INTERNAL".into()));
        data.insert("message".into(), Value::String(message.into()));
        data.insert("retryable".into(), Value::Bool(false));
        self.response(Status::Error, data)
    }

    /// Build an error response from an [`ErrorCode`] implementor.
    #[must_use]
    pub fn error_from(&self, err: &(impl ErrorCode + ?Sized)) -> Self {
        let mut data = HashMap::new();
        data.insert("code".into(), Value::String(err.error_code().into()));
        data.insert("message".into(), Value::String(err.to_string()));
        data.insert("retryable".into(), Value::Bool(err.retryable()));
        self.response(Status::Error, data)
    }

    /// Build a cancel frame targeting this request.
    #[must_use]
    pub fn cancel(&self) -> Self {
        self.response(Status::Cancel, HashMap::new())
    }

    // ── Builders ──

    /// Set the `from` field.
    #[must_use]
    pub fn with_from(mut self, from: impl Into<String>) -> Self {
        self.from = Some(from.into());
        self
    }

    /// Set the `trace` field.
    #[must_use]
    pub fn with_trace(mut self, trace: Value) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Set a single key-value pair in `data` using a raw `Value`.
    #[must_use]
    pub fn with_data(mut self, key: impl Into<String>, value: Value) -> Self {
        self.data.insert(key.into(), value);
        self
    }

    /// Set a single key-value pair in `data` from a serializable value.
    ///
    /// Avoids requiring callers to import `serde_json::Value` or construct
    /// `Value` variants manually.
    pub fn with_field(
        mut self,
        key: impl Into<String>,
        value: impl Serialize,
    ) -> Result<Self, serde_json::Error> {
        let json = serde_json::to_value(value)?;
        self.data.insert(key.into(), json);
        Ok(self)
    }

    // ── Queries ──

    /// Extract the call prefix (e.g., `"vfs:read"` → `"vfs"`).
    #[must_use]
    pub fn prefix(&self) -> &str {
        self.call
            .split_once(':')
            .map_or(&self.call, |(prefix, _)| prefix)
    }

    /// Extract the call verb (e.g., `"vfs:read"` → `"read"`).
    #[must_use]
    pub fn verb(&self) -> &str {
        self.call.split_once(':').map_or("", |(_, verb)| verb)
    }

    // ── Internal ──

    /// Shared construction logic for all response frame variants.
    ///
    /// Sets `parent_id = self.id` to establish the correlation that the router
    /// uses to route responses back to the correct subscribers.
    ///
    /// WHY `trace` is cloned: Observability metadata (span context, room, timing)
    /// must flow from request to response so that the gateway layer can associate
    /// all frames in a conversation. Cloning here means handlers never need to
    /// thread `trace` through manually.
    ///
    /// WHY `from` is `None`: Response frames represent the subsystem's reply,
    /// not the original caller's identity. Subsystems that want to identify
    /// themselves in responses can call `.with_from()` after construction.
    fn response(&self, status: Status, data: Data) -> Self {
        Self {
            id: Uuid::new_v4(),
            parent_id: Some(self.id),
            created_ms: now_millis(),
            expires_in: 0,
            from: None,
            call: self.call.clone(),
            status,
            trace: self.trace.clone(),
            data,
        }
    }
}

/// Convert a serializable value into a flat `Data` map.
///
/// If the value serializes to a JSON object, returns its fields.
/// Otherwise wraps the value under a `"value"` key.
pub fn to_data<T: Serialize>(value: &T) -> Result<Data, serde_json::Error> {
    let json = serde_json::to_value(value)?;
    Ok(match json {
        Value::Object(map) => map.into_iter().collect(),
        other => {
            let mut data = Data::new();
            data.insert("value".into(), other);
            data
        }
    })
}

/// Returns the current time as milliseconds since the Unix epoch.
///
/// Returns 0 if the system clock is before the epoch (unreachable in practice)
/// and `i64::MAX` if the duration overflows `i64` (not until year ~292 million).
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
#[path = "frame_test.rs"]
mod tests;
