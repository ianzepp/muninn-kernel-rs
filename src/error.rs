//! Kernel error types.
//!
//! This module defines three error families:
//!
//! - [`KernelError`] — structured errors produced by the kernel itself (routing
//!   failures, queue overflows, cancellations). Implements [`ErrorCode`] so it
//!   can be embedded directly into error frames via [`Frame::error_from`].
//!
//! - [`PipeError`] — errors from pipe send/receive operations (channel closed,
//!   send failed, serialization failure).
//!
//! - [`SigcallError`] — errors from the sigcall registry (already registered,
//!   not registered, ownership violation, reserved prefix).
//!
//! # Error Code Conventions
//!
//! All kernel error codes use the `E_` prefix and are uppercase snake-case
//! (e.g., `"E_NOT_FOUND"`, `"E_TIMEOUT"`). Subsystems define their own codes
//! following the same convention. The `retryable` flag signals whether the
//! caller should retry the operation — only `E_TIMEOUT` is retryable by default.

use std::collections::HashMap;

use serde_json::Value;

use crate::frame::Data;

/// Structured kernel error with code, message, and optional metadata.
///
/// Implements [`ErrorCode`](crate::frame::ErrorCode) so it can be converted
/// directly into an error frame via [`Frame::error_from`](crate::frame::Frame::error_from).
/// Use the factory methods to create well-typed errors; avoid constructing
/// `KernelError` fields directly to keep codes consistent.
#[derive(Clone, Debug)]
pub struct KernelError {
    /// Machine-readable error code (e.g., `"E_NOT_FOUND"`).
    pub code: &'static str,
    /// Human-readable error description for logging and display.
    pub message: String,
    /// Whether the caller should retry this operation.
    pub retryable: bool,
}

impl KernelError {
    /// Request arguments are missing, malformed, or out of range.
    pub fn invalid_args(message: impl Into<String>) -> Self {
        Self {
            code: "E_INVALID_ARGS",
            message: message.into(),
            retryable: false,
        }
    }

    /// The requested resource or entity does not exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: "E_NOT_FOUND",
            message: message.into(),
            retryable: false,
        }
    }

    /// The caller does not have permission to perform this operation.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            code: "E_FORBIDDEN",
            message: message.into(),
            retryable: false,
        }
    }

    /// The operation was cancelled by the caller.
    ///
    /// Used by `register_syscall` handlers that observe `CancellationToken`
    /// and return early. The kernel uses this code when routing cancel frames
    /// back to subscribers.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            code: "E_CANCELLED",
            message: "operation cancelled".into(),
            retryable: false,
        }
    }

    /// The operation exceeded a time or queue limit.
    ///
    /// Marked retryable because the failure is transient — the subsystem was
    /// temporarily overloaded rather than fundamentally broken.
    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            code: "E_TIMEOUT",
            message: message.into(),
            retryable: true,
        }
    }

    /// An unexpected internal error occurred.
    ///
    /// Use this for programming errors, failed invariants, or any condition
    /// that should not happen under normal operation.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "E_INTERNAL",
            message: message.into(),
            retryable: false,
        }
    }

    /// No handler found for the requested syscall.
    ///
    /// Produced by the router when neither the built-in route table nor the
    /// sigcall registry contains a handler for the requested prefix/syscall.
    /// Also produced when a registered route's channel has been closed.
    pub fn no_route(message: impl Into<String>) -> Self {
        Self {
            code: "E_NO_ROUTE",
            message: message.into(),
            retryable: false,
        }
    }

    /// Convert to a flat data map suitable for a Frame's data field.
    #[must_use]
    pub fn to_data(&self) -> Data {
        let mut map = HashMap::new();
        map.insert("code".into(), Value::String(self.code.into()));
        map.insert("message".into(), Value::String(self.message.clone()));
        map.insert("retryable".into(), Value::Bool(self.retryable));
        map
    }
}

impl std::fmt::Display for KernelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for KernelError {}

impl crate::frame::ErrorCode for KernelError {
    fn error_code(&self) -> &'static str {
        self.code
    }

    fn retryable(&self) -> bool {
        self.retryable
    }
}

/// Errors from pipe operations.
#[derive(Debug, thiserror::Error)]
pub enum PipeError {
    #[error("channel closed")]
    Closed,
    #[error("send failed: channel full or closed")]
    SendFailed,
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Errors from sigcall registry operations.
#[derive(Debug, thiserror::Error)]
pub enum SigcallError {
    #[error("sigcall {name:?} already registered by {owner:?}")]
    AlreadyRegistered { name: String, owner: String },
    #[error("sigcall {name:?} not registered")]
    NotRegistered { name: String },
    #[error("sigcall {name:?} owned by {owner:?}, not {caller:?}")]
    NotOwner {
        name: String,
        owner: String,
        caller: String,
    },
    #[error("cannot register reserved prefix: {name:?}")]
    Reserved { name: String },
}
