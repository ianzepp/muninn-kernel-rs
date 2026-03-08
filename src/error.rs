//! Kernel error types.

use std::collections::HashMap;

use serde_json::Value;

use crate::frame::Data;

/// Structured kernel error with code, message, and optional metadata.
#[derive(Clone, Debug)]
pub struct KernelError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl KernelError {
    pub fn invalid_args(message: impl Into<String>) -> Self {
        Self {
            code: "E_INVALID_ARGS",
            message: message.into(),
            retryable: false,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: "E_NOT_FOUND",
            message: message.into(),
            retryable: false,
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            code: "E_FORBIDDEN",
            message: message.into(),
            retryable: false,
        }
    }

    #[must_use] 
    pub fn cancelled() -> Self {
        Self {
            code: "E_CANCELLED",
            message: "operation cancelled".into(),
            retryable: false,
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            code: "E_TIMEOUT",
            message: message.into(),
            retryable: true,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "E_INTERNAL",
            message: message.into(),
            retryable: false,
        }
    }

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
