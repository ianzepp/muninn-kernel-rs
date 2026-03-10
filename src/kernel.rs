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

use crate::backpressure::{BackpressureConfig, StreamController, Subscriber};
use crate::frame::{Frame, Status};
use crate::pipe::{self, PipeEnd};
use crate::router::Router;
use crate::sender::FrameSender;
use crate::sigcall::SigcallRegistry;
use crate::syscall::Syscall;

const CHANNEL_BUFFER: usize = 256;

/// A callback registered per-prefix to trigger a `CancellationToken` by request ID.
///
/// The router calls this hook when a `Cancel` frame arrives for an active request
/// handled by a `Syscall` trait implementor. The hook's closure looks up the
/// token in the subsystem's `active_tokens` map and calls `cancel()` on it.
///
/// WHY: The `CancellationToken` lives inside the spawned handler task and cannot
/// be stored directly in the router (it would require `Arc<Mutex>` or a channel).
/// The hook pattern keeps the router's cancel logic simple (one `Arc<dyn Fn>` call)
/// while letting the subsystem loop manage its own token lifecycle.
pub(crate) type CancelHook = Arc<dyn Fn(Uuid) + Send + Sync>;

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
    cancel_hooks: HashMap<String, CancelHook>,
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
            cancel_hooks: HashMap::new(),
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
    /// Internally this method:
    /// 1. Creates a pipe and registers its kernel-side end in the route table.
    /// 2. Registers a `CancelHook` that the router can call to trigger the
    ///    `CancellationToken` for any in-flight request.
    /// 3. Spawns a receive loop that calls `handler.dispatch` once per `Request`
    ///    frame, each in its own task with a dedicated `CancellationToken`.
    ///
    /// # Cancellation Token Lifecycle
    ///
    /// Each request gets a fresh `CancellationToken` stored in `active_tokens`
    /// (keyed by request ID). The token is removed from the map when:
    /// - The handler task completes (normal exit or error).
    /// - A `Cancel` frame arrives (via the hook or directly in the receive loop).
    ///
    /// The token can be triggered by two paths:
    /// - The router's `CancelHook` (called from `route_cancel`).
    /// - The subsystem's own receive loop (when a `Cancel` frame reaches it
    ///   after the router has already forwarded it).
    ///
    /// Both paths are idempotent: `CancellationToken::cancel` is safe to call
    /// multiple times, and `HashMap::remove` on an already-removed key is a no-op.
    pub fn register_syscall(&mut self, handler: Arc<dyn Syscall>) {
        let prefix = handler.prefix();
        let mut sub_end = self.register(prefix);

        // Shared token map: the cancel hook closure and the receive loop both
        // hold a clone of this Arc to reach it from different contexts.
        let active_tokens: Arc<Mutex<HashMap<Uuid, CancellationToken>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let cancel_tokens = Arc::clone(&active_tokens);

        // Register the hook the router will call when a Cancel frame arrives.
        // The hook receives the target request ID and triggers its token.
        self.cancel_hooks.insert(
            prefix.to_string(),
            Arc::new(move |request_id| {
                let mut guard = cancel_tokens
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(token) = guard.remove(&request_id) {
                    token.cancel();
                }
            }),
        );

        tokio::spawn(async move {
            let sender = FrameSender::new(sub_end.sender());
            // WHY: caller() is called once here, not per-request. The single
            // Caller is cloned for each handler task. This avoids re-triggering
            // the lazy dispatcher on every request.
            let caller = sub_end.caller();

            while let Some(frame) = sub_end.recv().await {
                match frame.status {
                    Status::Cancel => {
                        // EDGE: The router already triggered the cancel hook
                        // before forwarding this frame. Handle it here too as
                        // a defensive measure in case the hook path was missed.
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

                        // Insert before spawning so the token is available if
                        // a Cancel frame arrives before the task starts.
                        {
                            let mut guard = tokens
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            guard.insert(frame.id, cancel.clone());
                        }

                        let request_id = frame.id;
                        tokio::spawn(async move {
                            let result = handler.dispatch(&frame, &sender, &caller, cancel).await;
                            // Auto-send error frame if handler returned Err,
                            // removing boilerplate from the common single-error case.
                            if let Err(err) = result {
                                let _ = sender.send(frame.error_from(&*err)).await;
                            }
                            // Clean up the token entry regardless of how the
                            // handler finished (ok, error, or panic-unwind).
                            let mut guard = tokens
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            guard.remove(&request_id);
                        });
                    }
                    _ => {} // Response frames are handled by the pipe dispatcher
                }
            }
        });
    }

    /// Subscribe to receive response frames routed through the kernel.
    ///
    /// Returns a [`Subscriber`] that acknowledges frames as they are consumed.
    /// Must be called before [`start`](Kernel::start).
    ///
    /// # Future: Subscriber type
    ///
    /// Currently returns a lightweight [`Subscriber`] wrapper. A future version
    /// may provide:
    /// - Filtering by `parent_id` or syscall prefix (connection-scoped delivery)
    /// - Typed deserialization helpers (`recv_as::<T>()`)
    /// - Backpressure acknowledgment signals back to the `StreamController`
    /// - Automatic reconnect/resubscribe on channel close
    ///
    /// For now the wrapper only adds backpressure acknowledgements. Gateway
    /// code still handles filtering and serialization at the boundary layer.
    pub fn subscribe(&mut self) -> Subscriber {
        let (tx, rx) = mpsc::channel(CHANNEL_BUFFER);
        let controller = StreamController::new(tx, self.backpressure.clone());
        self.subscribers.push(controller.clone());
        Subscriber::new(rx, controller)
    }

    /// Start the kernel event loop. Consumes self.
    ///
    /// PHASE 1: PIPE BRIDGING
    /// Each registered pipe has two ends: the subsystem holds one end, and the
    /// kernel holds the other. The kernel-side end receives response frames
    /// from subsystems. These must be forwarded to the shared inbound channel
    /// so the router can route them to the correct subscribers. One lightweight
    /// bridge task is spawned per registered subsystem.
    ///
    /// WHY: Subsystems write responses to their local pipe rather than directly
    /// to the inbound channel. This decouples subsystems from the kernel's
    /// internal channel and keeps the pipe abstraction clean.
    ///
    /// PHASE 2: ROUTER SPAWN
    /// The router is created with all routing state and spawned as the single
    /// event loop task. The kernel's `inbound_rx` is handed off — it cannot be
    /// used after this point (enforced by the `take().expect` call).
    #[must_use]
    pub fn start(mut self) -> JoinHandle<()> {
        let mut inbound_rx = self.inbound_rx.take().expect("kernel already started");
        let inbound_tx = self.inbound_tx.clone();

        // Spawn one bridge task per subsystem pipe. Each task forwards all
        // frames from the kernel-side pipe end to the shared inbound channel.
        for (_kernel_tx, mut kernel_end) in self.pipe_ends {
            let tx = inbound_tx.clone();
            tokio::spawn(async move {
                while let Some(frame) = kernel_end.recv().await {
                    if tx.send(frame).await.is_err() {
                        break; // Inbound channel closed — kernel is shutting down
                    }
                }
            });
        }

        let mut router = Router::new(
            self.routes,
            self.cancel_hooks,
            self.subscribers,
            self.sigcalls,
        );

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
