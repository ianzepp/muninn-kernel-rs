//! Router — runtime frame dispatcher for the kernel.
//!
//! The router owns the event loop and all routing state (`pending`, `active`).
//! It receives references to the route table, subscribers, and sigcall registry
//! from the kernel at construction time. The kernel is the builder; the router
//! is the runtime.

use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::backpressure::StreamController;
use crate::error::KernelError;
use crate::frame::{Frame, Status};
use crate::sigcall::SigcallRegistry;

/// Entry tracking an active (in-flight) request for cancel forwarding.
struct ActiveEntry {
    subsystem_tx: mpsc::Sender<Frame>,
    cancel: CancellationToken,
}

type PendingRoutes = HashMap<Uuid, Vec<StreamController>>;
type ActiveRequests = HashMap<Uuid, ActiveEntry>;

/// Runtime frame dispatcher. Created by [`Kernel::start`](crate::kernel::Kernel::start)
/// and runs as a spawned task.
pub(crate) struct Router {
    routes: HashMap<String, mpsc::Sender<Frame>>,
    subscribers: Vec<StreamController>,
    sigcalls: SigcallRegistry,
    pending: PendingRoutes,
    active: ActiveRequests,
}

impl Router {
    /// Create a new router with the given routing state.
    pub(crate) fn new(
        routes: HashMap<String, mpsc::Sender<Frame>>,
        subscribers: Vec<StreamController>,
        sigcalls: SigcallRegistry,
    ) -> Self {
        Self {
            routes,
            subscribers,
            sigcalls,
            pending: HashMap::new(),
            active: HashMap::new(),
        }
    }

    /// Run the event loop, consuming frames from the inbound channel.
    pub(crate) async fn run(&mut self, rx: &mut mpsc::Receiver<Frame>) {
        while let Some(frame) = rx.recv().await {
            match frame.status {
                Status::Request => self.route_request(&frame).await,
                Status::Cancel => self.route_cancel(&frame),
                _ => self.route_response(&frame).await,
            }
        }
    }

    // ── Request routing ──

    async fn route_request(&mut self, frame: &Frame) {
        // Handle sigcall management syscalls inline
        if frame.prefix() == "sigcall" {
            self.handle_sigcall_management(frame).await;
            return;
        }

        // Find the subsystem channel: built-in routes first, then sigcall registry
        let subsystem_tx = self
            .routes
            .get(frame.prefix())
            .cloned()
            .or_else(|| self.sigcalls.lookup(&frame.syscall));

        let Some(subsystem_tx) = subsystem_tx else {
            let err =
                KernelError::no_route(format!("no handler for syscall: {}", frame.syscall));
            let error_frame = frame.error_from(&err);
            for sub in &self.subscribers {
                sub.send(&error_frame).await;
            }
            return;
        };

        // Register response destinations
        self.pending.insert(
            frame.id,
            self.subscribers.iter().map(StreamController::clone).collect(),
        );

        // Track active request for cancel forwarding
        let cancel = CancellationToken::new();
        self.active.insert(
            frame.id,
            ActiveEntry {
                subsystem_tx: subsystem_tx.clone(),
                cancel,
            },
        );

        // Forward to subsystem
        let _ = subsystem_tx.send(frame.clone()).await;
    }

    // ── Response routing ──

    async fn route_response(&mut self, frame: &Frame) {
        let Some(parent_id) = frame.parent_id else {
            return;
        };

        let Some(controllers) = self.pending.get(&parent_id) else {
            return;
        };

        for ctrl in controllers {
            ctrl.send(frame).await;
        }

        if frame.status.is_terminal() {
            self.pending.remove(&parent_id);
            self.active.remove(&parent_id);
        }
    }

    // ── Cancel routing ──

    fn route_cancel(&mut self, frame: &Frame) {
        let Some(target_id) = frame.parent_id else {
            return;
        };

        if let Some(entry) = self.active.remove(&target_id) {
            entry.cancel.cancel();
            let _ = entry.subsystem_tx.try_send(frame.clone());
        }

        self.pending.remove(&target_id);
    }

    // ── Inline sigcall management ──

    async fn handle_sigcall_management(&mut self, frame: &Frame) {
        let verb = frame.verb();
        let response = match verb {
            "list" => {
                let entries = self.sigcalls.list();
                for (name, owner) in &entries {
                    let mut data = crate::frame::Data::new();
                    data.insert("name".into(), serde_json::Value::String(name.clone()));
                    data.insert("owner".into(), serde_json::Value::String(owner.clone()));
                    let item = frame.item(data);
                    for sub in &self.subscribers {
                        sub.send(&item).await;
                    }
                }
                frame.done()
            }
            "register" | "unregister" => {
                frame.error("sigcall:register and sigcall:unregister must be called through the SigcallRegistry API directly")
            }
            _ => frame.error(format!("unknown sigcall operation: {verb}")),
        };

        // Register pending so the response routes correctly
        self.pending.insert(
            frame.id,
            self.subscribers.iter().map(StreamController::clone).collect(),
        );

        for sub in &self.subscribers {
            sub.send(&response).await;
        }

        if response.status.is_terminal() {
            self.pending.remove(&frame.id);
        }
    }
}
