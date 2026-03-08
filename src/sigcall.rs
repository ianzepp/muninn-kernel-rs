//! Sigcall registry — dynamic handler registration for inverse syscalls.
//!
//! Sigcalls allow external handlers (gateway processes, plugins, services)
//! to register as handlers for specific syscall names at runtime. When the
//! kernel receives a request it can't route to a built-in subsystem, it
//! checks the sigcall registry.
//!
//! ```text
//! Syscall:  caller → kernel → built-in handler → kernel → caller
//! Sigcall:  caller → kernel → registered handler → kernel → caller
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::error::SigcallError;
use crate::frame::Frame;

/// A registered sigcall handler.
struct Registration {
    owner: String,
    tx: mpsc::Sender<Frame>,
}

/// Registry of dynamically registered sigcall handlers.
///
/// Thread-safe: uses internal `Mutex` for concurrent access.
#[derive(Clone)]
pub struct SigcallRegistry {
    inner: Arc<Mutex<HashMap<String, Registration>>>,
}

impl SigcallRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a handler for a syscall name.
    ///
    /// Returns an error if the name is already registered by a different owner.
    /// Re-registering the same name by the same owner updates the sender.
    pub fn register(
        &self,
        name: &str,
        owner: &str,
        tx: mpsc::Sender<Frame>,
    ) -> Result<(), SigcallError> {
        // Prevent registration of kernel-reserved prefixes
        if name.starts_with("sigcall:") {
            return Err(SigcallError::Reserved { name: name.into() });
        }

        let mut guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(existing) = guard.get(name) {
            if existing.owner != owner {
                return Err(SigcallError::AlreadyRegistered {
                    name: name.into(),
                    owner: existing.owner.clone(),
                });
            }
        }

        guard.insert(
            name.into(),
            Registration {
                owner: owner.into(),
                tx,
            },
        );
        Ok(())
    }

    /// Unregister a handler. Only the registering owner can unregister.
    pub fn unregister(&self, name: &str, owner: &str) -> Result<(), SigcallError> {
        let mut guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        let Some(existing) = guard.get(name) else {
            return Err(SigcallError::NotRegistered { name: name.into() });
        };

        if existing.owner != owner {
            return Err(SigcallError::NotOwner {
                name: name.into(),
                owner: existing.owner.clone(),
                caller: owner.into(),
            });
        }

        guard.remove(name);
        Ok(())
    }

    /// Remove all registrations for a given owner.
    ///
    /// Called on process exit or connection close to prevent orphaned handlers.
    pub fn unregister_all(&self, owner: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|_, reg| reg.owner != owner);
    }

    /// Look up a handler for a syscall name.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<mpsc::Sender<Frame>> {
        let guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(name).map(|reg| reg.tx.clone())
    }

    /// List all registered handler names and their owners.
    #[must_use]
    pub fn list(&self) -> Vec<(String, String)> {
        let guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .iter()
            .map(|(name, reg)| (name.clone(), reg.owner.clone()))
            .collect()
    }

    /// Number of registered handlers.
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.len()
    }

    /// Returns `true` if the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SigcallRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "sigcall_test.rs"]
mod tests;
