//! Router — runtime frame dispatcher for the kernel.
//!
//! The router owns the event loop and all routing state (`pending`, `active`).
//! It receives the route table, subscribers, and sigcall registry from the kernel
//! at construction time. The kernel is the builder; the router is the runtime.
//!
//! # Routing Overview
//!
//! The router runs as a single spawned task, consuming frames from the shared
//! inbound channel. All routing state lives in plain `HashMap`s — no `Arc<Mutex>`
//! needed because only one task ever touches them.
//!
//! Frame dispatch branches on `status`:
//!
//! ```text
//! Request  → route_request  — find subsystem, record pending + active
//! Cancel   → route_cancel   — trigger token, forward to subsystem, fan out terminal
//! _        → route_response — fan out to pending subscribers, clean up on terminal
//! ```
//!
//! # State Lifecycle
//!
//! `pending` maps a request ID to the set of `StreamController`s that should
//! receive its response frames. `active` maps the same ID to the subsystem
//! channel and cancel hook needed to forward cancellations.
//!
//! Both maps are populated when a Request arrives and cleaned up when a terminal
//! frame is routed (`Done`, `Error`, or `Cancel`). Cleaning up on terminal
//! prevents memory leaks from long-lived sessions with many requests.
//!
//! # TRADE-OFFS
//!
//! **Single-task routing**: The router runs on one task and uses `try_send` for
//! subsystem delivery. This keeps the event loop non-blocking but means a full
//! subsystem queue fails fast with `E_TIMEOUT` rather than applying back-pressure
//! at the routing layer. The alternative (blocking send in the router) would stall
//! all other routes while one subsystem is slow.

use std::collections::HashMap;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::backpressure::StreamController;
use crate::error::KernelError;
use crate::frame::{Frame, Status};
use crate::kernel::CancelHook;
use crate::sigcall::SigcallRegistry;

/// Entry tracking an active (in-flight) request for cancel forwarding.
///
/// Stored in `active` for every in-flight request. When a `Cancel` frame
/// arrives, the router uses this entry to:
/// 1. Trigger the `CancellationToken` via `cancel_hook` (if the handler is a
///    `Syscall` trait implementor — raw `PipeEnd` subsystems manage their own tokens).
/// 2. Forward the cancel frame to the subsystem so it can clean up.
struct ActiveEntry {
    /// The subsystem channel to forward cancel frames to.
    subsystem_tx: mpsc::Sender<Frame>,
    /// Optional hook to trigger the `CancellationToken` for `Syscall` handlers.
    ///
    /// `None` for raw `PipeEnd` subsystems, which manage their own cancellation.
    cancel_hook: Option<CancelHook>,
}

/// Maps request ID → set of subscriber controllers waiting for its responses.
///
/// Populated when a Request is dispatched. Cleaned up on terminal frame delivery.
type PendingRoutes = HashMap<Uuid, Vec<StreamController>>;

/// Maps request ID → subsystem channel and cancel hook for an active request.
///
/// Populated alongside `PendingRoutes`. Used exclusively for cancel routing.
type ActiveRequests = HashMap<Uuid, ActiveEntry>;

/// Runtime frame dispatcher. Created by [`Kernel::start`](crate::kernel::Kernel::start)
/// and runs as a spawned task.
pub(crate) struct Router {
    routes: HashMap<String, mpsc::Sender<Frame>>,
    cancel_hooks: HashMap<String, CancelHook>,
    subscribers: Vec<StreamController>,
    sigcalls: SigcallRegistry,
    pending: PendingRoutes,
    active: ActiveRequests,
}

impl Router {
    /// Create a new router with the given routing state.
    pub(crate) fn new(
        routes: HashMap<String, mpsc::Sender<Frame>>,
        cancel_hooks: HashMap<String, CancelHook>,
        subscribers: Vec<StreamController>,
        sigcalls: SigcallRegistry,
    ) -> Self {
        Self {
            routes,
            cancel_hooks,
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
                Status::Cancel => self.route_cancel(&frame).await,
                _ => self.route_response(&frame).await,
            }
        }
    }

    // ── Request routing ──
    //
    // Handles the entry point for all new call invocations. Responsible for
    // resolving the destination subsystem and recording routing state so that
    // subsequent response and cancel frames can be delivered correctly.

    async fn route_request(&mut self, frame: &Frame) {
        // WHY: sigcall:* management is handled inline rather than routed to a
        // subsystem because it requires access to the router's own sigcall
        // registry, and passing a channel sender through a frame is not possible.
        if frame.prefix() == "sigcall" {
            self.handle_sigcall_management(frame).await;
            return;
        }

        // WHY: Built-in routes take priority over sigcall registrations. This
        // prevents external handlers from shadowing core kernel subsystems.
        let subsystem_tx = self
            .routes
            .get(frame.prefix())
            .cloned()
            .or_else(|| self.sigcalls.lookup(&frame.call));

        let Some(subsystem_tx) = subsystem_tx else {
            let err =
                KernelError::no_route(format!("no handler for call: {}", frame.call));
            let error_frame = frame.error_from(&err);
            self.deliver_to_subscribers(&error_frame).await;
            return;
        };

        // Record which subscribers should receive responses for this request.
        // Snapshot the current subscriber list — new subscribers added after
        // this point won't see responses for already-dispatched requests.
        self.pending.insert(
            frame.id,
            self.subscribers.iter().map(StreamController::clone).collect(),
        );

        // Record the subsystem channel so Cancel frames can be forwarded.
        self.active.insert(
            frame.id,
            ActiveEntry {
                subsystem_tx: subsystem_tx.clone(),
                cancel_hook: self.cancel_hooks.get(frame.prefix()).cloned(),
            },
        );

        // WHY: try_send keeps the event loop non-blocking. A full queue means
        // the subsystem can't keep up — fail fast with E_TIMEOUT rather than
        // stalling the router for all other routes. A closed channel means the
        // subsystem has gone away — fail fast with E_NO_ROUTE.
        match subsystem_tx.try_send(frame.clone()) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.pending.remove(&frame.id);
                self.active.remove(&frame.id);

                let err = KernelError::timeout(format!(
                    "subsystem queue full for call: {}",
                    frame.call
                ));
                self.deliver_to_subscribers(&frame.error_from(&err)).await;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.pending.remove(&frame.id);
                self.active.remove(&frame.id);

                let err = KernelError::no_route(format!(
                    "handler unavailable for call: {}",
                    frame.call
                ));
                self.deliver_to_subscribers(&frame.error_from(&err)).await;
            }
        }
    }

    // ── Response routing ──
    //
    // Handles Item, Bulk, Done, and Error frames produced by subsystems.
    // Fans out to all subscribers registered when the originating Request was
    // dispatched, then cleans up state on terminal frames.

    async fn route_response(&mut self, frame: &Frame) {
        // Frames without parent_id have no registered destination — silently
        // drop them. This guards against malformed frames from external code.
        let Some(parent_id) = frame.parent_id else {
            return;
        };

        // If there is no pending entry, the request was already terminated
        // (e.g., cancelled) or never existed. Drop the frame.
        let Some(controllers) = self.pending.get(&parent_id) else {
            return;
        };

        // Fan-out: every subscriber that was registered at dispatch time
        // receives this response frame. StreamController applies per-stream
        // backpressure during delivery.
        for ctrl in controllers {
            ctrl.send(frame).await;
        }

        // WHY: Clean up both maps together on terminal to prevent unbounded
        // growth. active is no longer needed once the stream is closed (there
        // is nothing left to cancel). pending is no longer needed because no
        // further responses will arrive.
        if frame.status.is_terminal() {
            self.pending.remove(&parent_id);
            self.active.remove(&parent_id);
        }
    }

    /// Deliver a frame to all current subscribers, bypassing the pending map.
    ///
    /// Used for error frames generated by the router itself (no-route errors,
    /// full-queue errors) and for inline sigcall management responses. These
    /// frames don't have a pending entry because the router synthesized them
    /// rather than forwarding them from a subsystem.
    async fn deliver_to_subscribers(&self, frame: &Frame) {
        for sub in &self.subscribers {
            sub.send(frame).await;
        }
    }

    // ── Cancel routing ──
    //
    // Cancellation is cooperative: the router triggers the CancellationToken
    // and forwards the cancel frame to the subsystem, but the subsystem is
    // responsible for observing the token and stopping work. The router then
    // immediately delivers a terminal Cancel frame to all subscribers so they
    // are unblocked without waiting for the subsystem to wind down.

    async fn route_cancel(&mut self, frame: &Frame) {
        // A cancel without parent_id has no target — drop it.
        let Some(target_id) = frame.parent_id else {
            return;
        };

        // Trigger the CancellationToken (for Syscall handlers) and forward
        // the cancel frame to the subsystem so it can perform cleanup.
        // remove() is intentional: the active entry is consumed — no further
        // cancel should be forwarded after the first one.
        if let Some(entry) = self.active.remove(&target_id) {
            if let Some(cancel_hook) = entry.cancel_hook {
                cancel_hook(target_id);
            }
            // EDGE: try_send here because the subsystem may already be shutting
            // down. Failure to deliver the cancel frame is acceptable — the
            // token is already triggered.
            let _ = entry.subsystem_tx.try_send(frame.clone());
        }

        // Remove pending entry and deliver a terminal Cancel frame to all
        // subscribers. This unblocks callers immediately rather than making
        // them wait for the subsystem to observe cancellation and send its
        // own terminal frame.
        let Some(controllers) = self.pending.remove(&target_id) else {
            return;
        };

        // WHY: Construct a new cancel response with a fresh ID rather than
        // forwarding the original cancel frame verbatim. The original frame
        // has the request's ID as parent_id, but we need to emit a response
        // (parent_id = target_id) that subscribers can correlate back to the
        // original request. We also clear data to keep the cancel signal clean.
        let mut cancel_response = frame.clone();
        cancel_response.id = Uuid::new_v4();
        cancel_response.parent_id = Some(target_id);
        cancel_response.status = Status::Cancel;
        cancel_response.data.clear();

        for ctrl in &controllers {
            ctrl.send(&cancel_response).await;
        }
    }

    // ── Inline sigcall management ──
    //
    // sigcall:* requests are handled by the router itself rather than routed to
    // a subsystem. This is necessary because:
    //  - sigcall:list needs access to the live registry state inside the router.
    //  - sigcall:register/unregister require passing an mpsc::Sender, which cannot
    //    be carried in a frame's data payload.
    //
    // The router acts as both dispatcher and handler for this special prefix.

    async fn handle_sigcall_management(&mut self, frame: &Frame) {
        let verb = frame.verb();
        let response = match verb {
            // Stream one Item per registered handler, then Done.
            "list" => {
                let entries = self.sigcalls.list();
                for (name, owner) in &entries {
                    let mut data = crate::frame::Data::new();
                    data.insert("name".into(), serde_json::Value::String(name.clone()));
                    data.insert("owner".into(), serde_json::Value::String(owner.clone()));
                    let item = frame.item(data);
                    self.deliver_to_subscribers(&item).await;
                }
                frame.done()
            }
            // These verbs require out-of-band channel passing; reject them here
            // and direct callers to the SigcallRegistry API.
            "register" | "unregister" => {
                frame.error("sigcall:register and sigcall:unregister must be called through the SigcallRegistry API directly")
            }
            _ => frame.error(format!("unknown sigcall operation: {verb}")),
        };

        // WHY: Register pending before delivering the terminal response so the
        // response is routed correctly. Without this, the pending lookup in
        // route_response would find nothing and drop the frame.
        self.pending.insert(
            frame.id,
            self.subscribers.iter().map(StreamController::clone).collect(),
        );

        self.deliver_to_subscribers(&response).await;

        // Clean up immediately — inline handlers always produce a single
        // terminal response (the Item frames above bypass pending entirely).
        if response.status.is_terminal() {
            self.pending.remove(&frame.id);
        }
    }
}
