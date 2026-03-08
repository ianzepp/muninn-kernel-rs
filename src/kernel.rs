//! Kernel — builder for subsystem registration and wiring.
//!
//! The kernel owns configuration and registration. When [`Kernel::start`] is
//! called, it creates a [`Router`](crate::router::Router) and hands off all
//! routing state. The kernel is the builder; the router is the runtime.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::backpressure::{BackpressureConfig, StreamController};
use crate::frame::{Frame, Status};
use crate::pipe::{self, PipeEnd};
use crate::router::Router;
use crate::sender::FrameSender;
use crate::sigcall::SigcallRegistry;
use crate::syscall::Syscall;

const CHANNEL_BUFFER: usize = 256;

/// The microkernel: registers subsystems, then starts a router event loop.
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

        tokio::spawn(async move {
            let sender = FrameSender::new(sub_end.sender());
            let caller = sub_end.caller();

            // Track active requests so Cancel frames can trigger tokens
            let active_tokens: Arc<Mutex<HashMap<Uuid, CancellationToken>>> =
                Arc::new(Mutex::new(HashMap::new()));

            while let Some(frame) = sub_end.recv().await {
                match frame.status {
                    Status::Cancel => {
                        // Trigger the token for the target request
                        if let Some(target_id) = frame.parent_id {
                            let mut guard = active_tokens
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(token) = guard.remove(&target_id) {
                                token.cancel();
                            }
                        }
                    }
                    Status::Request => {
                        let handler = Arc::clone(&handler);
                        let sender = sender.clone();
                        let caller = caller.clone();
                        let cancel = CancellationToken::new();
                        let tokens = Arc::clone(&active_tokens);

                        // Store token so Cancel frames can find it
                        {
                            let mut guard = tokens
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            guard.insert(frame.id, cancel.clone());
                        }

                        let request_id = frame.id;
                        tokio::spawn(async move {
                            let result =
                                handler.dispatch(&frame, &sender, &caller, cancel).await;
                            // Auto-send error frame if handler returned Err
                            if let Err(err) = result {
                                let _ = sender.send(frame.error_from(&*err)).await;
                            }
                            // Clean up token on completion
                            let mut guard = tokens
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            guard.remove(&request_id);
                        });
                    }
                    _ => {} // Responses are handled by the pipe dispatcher
                }
            }
        });
    }

    /// Subscribe to receive response frames routed through the kernel.
    ///
    /// Returns a raw receiver channel. Must be called before [`start`](Kernel::start).
    ///
    /// # Future: Subscriber type
    ///
    /// Currently returns a bare `mpsc::Receiver<Frame>`. A future `Subscriber`
    /// wrapper may provide:
    /// - Filtering by `parent_id` or syscall prefix (connection-scoped delivery)
    /// - Typed deserialization helpers (`recv_as::<T>()`)
    /// - Backpressure acknowledgment signals back to the `StreamController`
    /// - Automatic reconnect/resubscribe on channel close
    ///
    /// For now the raw receiver is sufficient. Gateway code should handle
    /// filtering and serialization at the boundary layer.
    pub fn subscribe(&mut self) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(CHANNEL_BUFFER);
        self.subscribers
            .push(StreamController::new(tx, self.backpressure.clone()));
        rx
    }

    /// Start the kernel event loop. Consumes self.
    ///
    /// Creates a [`Router`] and spawns it as the runtime event loop.
    /// Bridges each registered pipe back to the inbound channel.
    #[must_use]
    pub fn start(mut self) -> JoinHandle<()> {
        let mut inbound_rx = self.inbound_rx.take().expect("kernel already started");
        let inbound_tx = self.inbound_tx.clone();

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

        let mut router = Router::new(self.routes, self.subscribers, self.sigcalls);

        tokio::spawn(async move {
            router.run(&mut inbound_rx).await;
        })
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "kernel_test.rs"]
mod tests;
