//! Kernel — single event loop that routes frames between subsystems.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::backpressure::{BackpressureConfig, StreamController};
use crate::error::KernelError;
use crate::frame::{Frame, Status};
use crate::pipe::{self, PipeEnd};
use crate::sender::FrameSender;
use crate::sigcall::SigcallRegistry;
use crate::syscall::Syscall;

const CHANNEL_BUFFER: usize = 256;

type PendingRoutes = Arc<Mutex<HashMap<Uuid, Vec<StreamController>>>>;
type ActiveRequests = Arc<Mutex<HashMap<Uuid, ActiveEntry>>>;

struct ActiveEntry {
    subsystem_tx: mpsc::Sender<Frame>,
    cancel: CancellationToken,
}

/// The microkernel: a single event loop that routes frames by syscall prefix.
///
/// # Usage
///
/// 1. Create a kernel with [`Kernel::new`].
/// 2. Register subsystems with [`Kernel::register`] or [`Kernel::register_syscall`].
/// 3. Subscribe to response frames with [`Kernel::subscribe`].
/// 4. Start the event loop with [`Kernel::start`].
pub struct Kernel {
    inbound_tx: mpsc::Sender<Frame>,
    inbound_rx: Option<mpsc::Receiver<Frame>>,
    routes: HashMap<String, mpsc::Sender<Frame>>,
    pipe_ends: Vec<(mpsc::Sender<Frame>, PipeEnd)>,
    subscribers: Vec<StreamController>,
    sigcalls: SigcallRegistry,
    backpressure: BackpressureConfig,
}

impl Kernel {
    /// Create a new kernel.
    #[must_use]
    pub fn new() -> Self {
        Self::with_backpressure(BackpressureConfig::default())
    }

    /// Create a new kernel with custom backpressure configuration.
    #[must_use]
    pub fn with_backpressure(config: BackpressureConfig) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel(CHANNEL_BUFFER);
        Self {
            inbound_tx,
            inbound_rx: Some(inbound_rx),
            routes: HashMap::new(),
            pipe_ends: Vec::new(),
            subscribers: Vec::new(),
            sigcalls: SigcallRegistry::new(),
            backpressure: config,
        }
    }

    /// Get a cloneable sender for submitting frames to the kernel.
    #[must_use] 
    pub fn sender(&self) -> mpsc::Sender<Frame> {
        self.inbound_tx.clone()
    }

    /// Get a reference to the sigcall registry.
    #[must_use] 
    pub fn sigcalls(&self) -> &SigcallRegistry {
        &self.sigcalls
    }

    /// Register a subsystem to handle all syscalls with the given prefix.
    ///
    /// Returns a [`PipeEnd`] for the subsystem to receive requests and send
    /// responses. Must be called before [`start`](Kernel::start).
    pub fn register(&mut self, prefix: &str) -> PipeEnd {
        let (sub_end, kernel_end) = pipe::pipe(CHANNEL_BUFFER);
        let kernel_tx = kernel_end.sender();
        self.routes.insert(prefix.to_string(), kernel_tx.clone());
        self.pipe_ends.push((kernel_tx, kernel_end));
        sub_end
    }

    /// Register a [`Syscall`] trait implementor as a subsystem handler.
    ///
    /// Creates a pipe, spawns a receive loop that calls `dispatch` on each
    /// request, and wires up the `FrameSender` and Caller.
    pub fn register_syscall(&mut self, handler: Arc<dyn Syscall>) {
        let prefix = handler.prefix();
        let mut sub_end = self.register(prefix);
        let inbound_tx = self.inbound_tx.clone();

        tokio::spawn(async move {
            let sender = FrameSender::new(sub_end.sender());
            let caller = sub_end.caller();

            while let Some(frame) = sub_end.recv().await {
                if frame.status != Status::Request {
                    continue;
                }

                let handler = Arc::clone(&handler);
                let sender = sender.clone();
                let caller = caller.clone();
                let cancel = CancellationToken::new();
                let _inbound_tx = inbound_tx.clone();

                tokio::spawn(async move {
                    handler.dispatch(&frame, &sender, &caller, cancel).await;
                });
            }
        });
    }

    /// Subscribe to receive response frames routed through the kernel.
    ///
    /// Returns a receiver channel. Must be called before [`start`](Kernel::start).
    pub fn subscribe(&mut self) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(CHANNEL_BUFFER);
        self.subscribers.push(StreamController::new(tx, self.backpressure.clone()));
        rx
    }

    /// Start the kernel event loop. Consumes self.
    ///
    /// Spawns bridge tasks for each registered pipe and the main router loop.
    #[must_use] 
    pub fn start(mut self) -> JoinHandle<()> {
        let mut inbound_rx = self.inbound_rx.take().expect("kernel already started");
        let inbound_tx = self.inbound_tx.clone();
        let routes = self.routes;
        let subscribers = self.subscribers;
        let sigcalls = self.sigcalls;

        // Bridge each pipe's kernel-side end back to the inbound channel.
        for (_kernel_tx, mut kernel_end) in self.pipe_ends {
            let tx = inbound_tx.clone();
            tokio::spawn(async move {
                while let Some(frame) = kernel_end.recv().await {
                    if tx.send(frame).await.is_err() {
                        break;
                    }
                }
            });
        }

        let pending: PendingRoutes = Arc::new(Mutex::new(HashMap::new()));
        let active: ActiveRequests = Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(async move {
            router_loop(
                &mut inbound_rx,
                &routes,
                &subscribers,
                &sigcalls,
                &pending,
                &active,
            )
            .await;
        })
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

// ── Router ──

async fn router_loop(
    rx: &mut mpsc::Receiver<Frame>,
    routes: &HashMap<String, mpsc::Sender<Frame>>,
    subscribers: &[StreamController],
    sigcalls: &SigcallRegistry,
    pending: &PendingRoutes,
    active: &ActiveRequests,
) {
    while let Some(frame) = rx.recv().await {
        match frame.status {
            Status::Request => {
                route_request(&frame, routes, subscribers, sigcalls, pending, active).await;
            }
            Status::Cancel => {
                route_cancel(&frame, pending, active);
            }
            _ => {
                route_response(&frame, pending, active).await;
            }
        }
    }
}

async fn route_request(
    frame: &Frame,
    routes: &HashMap<String, mpsc::Sender<Frame>>,
    subscribers: &[StreamController],
    sigcalls: &SigcallRegistry,
    pending: &PendingRoutes,
    active: &ActiveRequests,
) {
    // Handle sigcall management syscalls inline
    if frame.prefix() == "sigcall" {
        handle_sigcall_management(frame, sigcalls, subscribers, pending).await;
        return;
    }

    // Find the subsystem channel: built-in routes first, then sigcall registry
    let subsystem_tx = routes
        .get(frame.prefix())
        .cloned()
        .or_else(|| sigcalls.lookup(&frame.syscall));

    let Some(subsystem_tx) = subsystem_tx else {
        let err = KernelError::no_route(format!("no handler for syscall: {}", frame.syscall));
        let error_frame = frame.error_from(&err);
        for sub in subscribers {
            sub.send(&error_frame).await;
        }
        return;
    };

    // Register response destinations
    {
        let mut guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(
            frame.id,
            subscribers.iter().map(StreamController::clone).collect(),
        );
    }

    // Track active request for cancel forwarding
    let cancel = CancellationToken::new();
    {
        let mut guard = active.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(
            frame.id,
            ActiveEntry {
                subsystem_tx: subsystem_tx.clone(),
                cancel,
            },
        );
    }

    // Forward to subsystem
    let _ = subsystem_tx.send(frame.clone()).await;
}

async fn route_response(frame: &Frame, pending: &PendingRoutes, active: &ActiveRequests) {
    let Some(parent_id) = frame.parent_id else {
        return;
    };

    let controllers = {
        let guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(&parent_id).cloned()
    };

    let Some(controllers) = controllers else {
        return;
    };

    for ctrl in &controllers {
        ctrl.send(frame).await;
    }

    if frame.status.is_terminal() {
        let mut guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&parent_id);

        let mut guard = active.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&parent_id);
    }
}

fn route_cancel(frame: &Frame, pending: &PendingRoutes, active: &ActiveRequests) {
    let Some(target_id) = frame.parent_id else {
        return;
    };

    let entry = {
        let mut guard = active.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&target_id)
    };

    if let Some(entry) = entry {
        // Trigger cancellation token
        entry.cancel.cancel();
        // Forward cancel frame to subsystem
        let _ = entry.subsystem_tx.try_send(frame.clone());
    }

    // Clean up pending
    {
        let mut guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&target_id);
    }
}

// ── Inline sigcall management ──

async fn handle_sigcall_management(
    frame: &Frame,
    sigcalls: &SigcallRegistry,
    subscribers: &[StreamController],
    pending: &PendingRoutes,
) {
    let verb = frame.verb();
    let response = match verb {
        "list" => {
            let entries = sigcalls.list();
            // Send each entry as an item, then done
            for (name, owner) in &entries {
                let mut data = crate::frame::Data::new();
                data.insert("name".into(), serde_json::Value::String(name.clone()));
                data.insert("owner".into(), serde_json::Value::String(owner.clone()));
                let item = frame.item(data);
                for sub in subscribers {
                    sub.send(&item).await;
                }
            }
            frame.done()
        }
        "register" | "unregister" => {
            // These require a sigcall channel to be passed, which happens
            // at the application layer (not inline). Return error for now
            // if called without proper context.
            frame.error("sigcall:register and sigcall:unregister must be called through the SigcallRegistry API directly")
        }
        _ => frame.error(format!("unknown sigcall operation: {verb}")),
    };

    // Register pending so the response routes correctly
    {
        let mut guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(
            frame.id,
            subscribers.iter().map(StreamController::clone).collect(),
        );
    }

    for sub in subscribers {
        sub.send(&response).await;
    }

    // Clean up
    if response.status.is_terminal() {
        let mut guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&frame.id);
    }
}

#[cfg(test)]
#[path = "kernel_test.rs"]
mod tests;
