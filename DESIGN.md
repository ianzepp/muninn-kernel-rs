# rust-kernel — Design Document

## Overview

A transport-agnostic microkernel for Rust applications that routes structured messages (frames) between subsystems using in-memory channels. No serialization occurs inside the kernel — frames are native Rust structs passed by value through `mpsc` channels. Serialization (protobuf, JSON, MessagePack) happens only at I/O boundaries (WebSocket, HTTP, TCP), handled by gateway code outside this crate.

This crate distills patterns from four prior projects into a single reusable library:

| Project | Language | Key contribution |
|---|---|---|
| **monk-os-kernel** | TypeScript | Message-pure kernel, sigcall registry, backpressure (StreamController) |
| **prior** | Rust | Pipe/Caller/CallStream, lazy dispatcher, PipeEnd abstraction, FrameSender helpers |
| **abbot** | Rust | Syscall trait, KernelDispatcher with lanes, SigcallHub (outbound delivery), backpressure watermarks |
| **gauntlet-week-1** | Rust | Outcome enum, protobuf Frame codec (extracted to `rust-frames`), prefix-based dispatch |

---

## Core Concepts

### Frame

The universal message type. All kernel communication uses `Frame` — requests, responses, streaming items, errors, cancellations. Frames are **never serialized inside the kernel**; they are Rust structs moved through channels.

```rust
pub struct Frame {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,    // Correlates responses to requests
    pub ts: i64,                     // Milliseconds since Unix epoch
    pub from: Option<String>,        // Sender identity (user ID, subsystem label)
    pub syscall: String,             // Namespaced operation: "prefix:verb"
    pub status: Status,              // Lifecycle position
    pub trace: Option<Value>,        // Observability metadata (separate from payload)
    pub data: Data,                  // Business payload: HashMap<String, Value>
}
```

**Design decisions:**
- `id` is `Uuid` (not `String`) inside the kernel — string conversion happens at the boundary.
- `data` is `HashMap<String, Value>` (flat key-value), not `serde_json::Value`. Subsystems deserialize to typed structs internally. This follows Prior's pattern where the kernel never inspects payloads — only routing prefixes.
- `trace` is kept separate from `data` so observability metadata (room, span, timing) doesn't pollute business payloads.
- No `board_id` or other domain-specific fields. The `from` field carries actor identity; room/context routing is handled via `trace` or `data` by the consumer.

### Status Lifecycle

```
Request → Item* / Bulk* → Done | Error | Cancel
```

| Status | Terminal? | Description |
|---|---|---|
| `Request` | No | Initiates a syscall |
| `Item` | No | Single streaming result |
| `Bulk` | No | Batch of streaming results |
| `Done` | Yes | Successful completion (may carry summary data) |
| `Error` | Yes | Failed completion (carries error code + message) |
| `Cancel` | Yes | Abort an in-flight request |

**Terminal status** means the response stream is complete. The kernel cleans up routing state (pending maps, active request tracking) when a terminal frame is received.

### Frame Construction Helpers

Frames are constructed via builder methods on a request frame, ensuring `parent_id` correlation:

```rust
impl Frame {
    // Create a new request
    pub fn request(syscall: &str) -> Self;
    pub fn request_with(syscall: &str, data: Data) -> Self;

    // Response constructors (set parent_id = self.id)
    pub fn item(&self, data: Data) -> Self;
    pub fn item_from<T: Serialize>(&self, value: &T) -> Self;
    pub fn bulk(&self, data: Data) -> Self;
    pub fn bulk_from<T: Serialize>(&self, value: &T) -> Self;
    pub fn done(&self) -> Self;
    pub fn done_with(&self, data: Data) -> Self;
    pub fn done_from<T: Serialize>(&self, value: &T) -> Self;
    pub fn error(&self, message: impl Into<String>) -> Self;
    pub fn error_from(&self, err: &(impl ErrorCode + ?Sized)) -> Self;
    pub fn cancel(&self) -> Self;

    // Builders
    pub fn with_from(self, from: impl Into<String>) -> Self;
    pub fn with_trace(self, trace: Value) -> Self;
    pub fn with_data(self, key: impl Into<String>, value: Value) -> Self;
    pub fn with_field(self, key: impl Into<String>, value: impl Serialize) -> Self;
}
```

The `_from` variants (`item_from`, `done_from`, `bulk_from`) serialize a struct directly into the frame's data map via `to_data()`. If the value serializes to a JSON object, its fields become data keys; otherwise the value is stored under a `"value"` key.

`with_field` accepts `impl Serialize` instead of `Value`, so handlers don't need to import `serde_json::Value` or manually construct `Value` variants.

### ErrorCode Trait

Subsystem errors implement this trait for structured error responses:

```rust
pub trait ErrorCode: std::fmt::Display {
    fn error_code(&self) -> &'static str;   // "E_NOT_FOUND", "E_PIPE", etc.
    fn retryable(&self) -> bool { false }
}
```

The `error_from` helper on Frame converts any `ErrorCode` implementor into a properly shaped error frame with `code`, `message`, and `retryable` fields in `data`.

---

## Architecture

### Kernel / Router Split

The crate separates the **builder** (Kernel) from the **runtime** (Router), following the pattern from monk-os-kernel where the kernel manages state and the dispatcher handles message flow.

**Kernel** — owns configuration, registration, and wiring. Consumed by `start()`.

**Router** — owns the event loop and routing state (`pending`, `active`). Created by `Kernel::start()` and runs as a single spawned task. The router's state is plain `HashMap` (no `Arc<Mutex>`) since it runs on one task.

```
  ┌──────────────────┐         ┌─────────────────────────────┐
  │   Kernel (setup)  │         │       Router (runtime)       │
  │                   │         │                              │
  │  routes           │──start──►  match frame.status {        │
  │  subscribers      │         │    Request => route_request  │
  │  sigcalls         │         │    Cancel  => route_cancel   │
  │  backpressure cfg │         │    _       => route_response │
  │  pipe_ends        │         │  }                           │
  └──────────────────┘         │                              │
                                │  routes: HashMap<prefix, tx> │
  inbound_tx ──────────────────►  pending: HashMap<id, [ctrl]> │
  (cloned to all)               │  active: HashMap<id, entry>  │
                                │  sigcalls: SigcallRegistry   │
                                └─────────────────────────────┘
                                     │           │
                          ┌──────────┘           └──────────┐
                          ▼                                  ▼
                    Subsystem A                        Subsystem B
                    (PipeEnd)                          (PipeEnd)
```

**Routing logic:**

1. **Request frames**: Extract prefix from `syscall` (e.g., `"vfs:read"` → `"vfs"`). Look up the subsystem channel in `routes`. If not found, check the sigcall registry. If still not found, send an error response back to all subscribers.

2. **Cancel frames**: Look up `parent_id` in `active` (which subsystem is handling the target request). Forward the cancel frame to that subsystem. Trigger the `CancellationToken`. Clean up `pending` and `active` entries.

3. **Response frames** (Item, Bulk, Done, Error): Look up `parent_id` in `pending` to find which `StreamController`-wrapped subscribers are waiting. Fan out the response with backpressure. On terminal status, remove entries from `pending` and `active`.

### Subsystem Registration

```rust
impl Kernel {
    /// Register a subsystem to handle all syscalls with the given prefix.
    /// Returns a PipeEnd for the subsystem to receive requests and send responses.
    pub fn register(&mut self, prefix: &str) -> PipeEnd;

    /// Register a Syscall trait implementor (auto-creates pipe and receive loop).
    pub fn register_syscall(&mut self, handler: Arc<dyn Syscall>);

    /// Subscribe to receive response frames (returns raw mpsc::Receiver).
    pub fn subscribe(&mut self) -> mpsc::Receiver<Frame>;

    /// Start the kernel event loop. Creates a Router and consumes self.
    pub fn start(self) -> JoinHandle<()>;
}
```

Registration happens **before** `start()`. Once the kernel is running, the routing table is immutable (subsystems are long-lived). Dynamic handler registration is handled by the sigcall registry (see below).

---

## Pipe Abstraction

Pipes are the transport layer for frames between subsystems and the kernel. A pipe is two crossed `mpsc` channels — each end can send and receive.

```rust
pub fn pipe(capacity: usize) -> (PipeEnd, PipeEnd);
```

### PipeEnd

Each subsystem gets one end of a pipe. It supports two modes of operation:

**Receive mode** (default): The subsystem calls `recv()` to get inbound request frames, processes them, and sends responses back via `send()`.

```rust
impl PipeEnd {
    /// Receive the next inbound frame.
    pub async fn recv(&mut self) -> Option<Frame>;

    /// Get a cloneable sender for sending frames back.
    pub fn sender(&self) -> mpsc::Sender<Frame>;

    /// Get a Caller for making outbound calls through the kernel.
    pub fn caller(&mut self) -> Caller;
}
```

**Call mode** (lazy): When a subsystem needs to call *another* subsystem through the kernel, it uses `caller()` to get a `Caller`, then calls `call()` which returns a correlated `CallStream`.

### Caller

A cloneable handle for making outbound syscall requests through the kernel and receiving correlated responses.

```rust
pub struct Caller { /* tx + pending map */ }

impl Caller {
    /// Send a request and receive a stream of correlated responses.
    pub async fn call(&self, request: Frame) -> Result<CallStream, PipeError>;

    /// Send a frame without expecting a response (fire-and-forget broadcast).
    pub async fn send(&self, frame: Frame) -> Result<(), PipeError>;
}
```

### CallStream

An owned stream of response frames correlated to a single request.

```rust
pub struct CallStream { /* rx + cleanup handle */ }

impl CallStream {
    /// Receive the next response frame.
    pub async fn recv(&mut self) -> Option<Frame>;

    /// Collect all frames until a terminal status, then return them.
    pub async fn collect(self) -> Vec<Frame>;
}
```

### Lazy Dispatcher Pattern (from Prior)

Most subsystems never use `call()` — they only receive requests and send responses. Spawning a dispatcher task for every pipe would waste resources. Instead, PipeEnd starts in **direct mode** (raw channel access) and transitions to **dispatched mode** only on the first `call()` invocation.

In direct mode, `recv()` reads directly from the channel. When `caller()` is first called, a dispatcher task is spawned that:
1. Takes ownership of the raw receiver
2. Routes incoming frames by `parent_id` to pending `CallStream` receivers
3. Routes unmatched frames to a default channel (which `recv()` now reads from)

This means zero overhead for subsystems that only handle requests.

---

## Syscall Trait

Subsystems that want a simpler API than raw PipeEnd can implement the Syscall trait:

```rust
#[async_trait]
pub trait Syscall: Send + Sync {
    /// The syscall prefix this handler owns (e.g., "vfs", "ems").
    fn prefix(&self) -> &'static str;

    /// Handle a single request frame. Emit responses via `tx`.
    async fn dispatch(
        &self,
        frame: &Frame,
        tx: &FrameSender,
        caller: &Caller,
        cancel: CancellationToken,
    ) -> Result<(), Box<dyn ErrorCode + Send>>;
}
```

The kernel provides a convenience method `register_syscall()` which internally creates a pipe, spawns a receive loop, and wires up the FrameSender and Caller.

### Error Handling

Returning `Err` from `dispatch` causes the kernel to automatically send an error frame on behalf of the handler via `frame.error_from(&*err)`. This removes boilerplate for the common case where a single error terminates the stream. Handlers that need to send partial results before failing should use `FrameSender` directly and return `Ok(())`.

### FrameSender

Helper for sending response frames with common patterns (from Prior's VFS):

```rust
pub struct FrameSender { /* mpsc::Sender<Frame> */ }

impl FrameSender {
    /// Send a single item, then done.
    pub async fn finish_item<T: Serialize>(&self, req: &Frame, result: Result<T, impl ErrorCode>);

    /// Send multiple items, then done.
    pub async fn finish_items<T: Serialize>(&self, req: &Frame, result: Result<Vec<T>, impl ErrorCode>);

    /// Send done or error based on result.
    pub async fn finish(&self, req: &Frame, result: Result<(), impl ErrorCode>);

    /// Send a single item frame.
    pub async fn send_item<T: Serialize>(&self, req: &Frame, value: &T);

    /// Send a done frame.
    pub async fn send_done(&self, req: &Frame);

    /// Send an error frame.
    pub async fn send_error(&self, req: &Frame, message: impl Into<String>);

    /// Send an error frame from an ErrorCode implementor.
    pub async fn send_error_from(&self, req: &Frame, err: &impl ErrorCode);
}
```

---

## Sigcall Registry

Sigcalls are the **inverse of syscalls**. Instead of the kernel dispatching to a built-in handler, sigcalls allow external handlers (userspace processes, gateway connections, plugins) to register as handlers for specific syscall names. When the kernel receives a request it can't route to a built-in subsystem, it checks the sigcall registry.

### Concept (from monk-os-kernel)

```
Syscall:  caller → kernel → built-in handler → kernel → caller
Sigcall:  caller → kernel → registered external handler → kernel → caller
```

This enables:
- **Plugin architecture**: External processes register handlers without modifying kernel code.
- **Service activation**: A gateway process registers `window:create`, and any caller invoking that syscall gets routed to the gateway handler transparently.
- **Dynamic dispatch**: Handlers can be registered and unregistered at runtime.

### Registry API

```rust
pub struct SigcallRegistry { /* HashMap<String, SigcallHandler> */ }

impl SigcallRegistry {
    /// Register a handler for a syscall name. Returns error if already registered
    /// by a different owner.
    pub fn register(&self, name: &str, owner: &str, tx: mpsc::Sender<Frame>) -> Result<(), SigcallError>;

    /// Unregister a handler. Only the registering owner can unregister.
    pub fn unregister(&self, name: &str, owner: &str) -> Result<(), SigcallError>;

    /// Remove all registrations for a given owner (cleanup on disconnect/exit).
    pub fn unregister_all(&self, owner: &str);

    /// Look up a handler for a syscall name.
    pub fn lookup(&self, name: &str) -> Option<mpsc::Sender<Frame>>;
}
```

### Routing Flow

When the kernel receives a Request frame and the prefix is not in the built-in routes table:

1. Check `sigcall_registry.lookup(syscall_name)` for an exact match.
2. If found, forward the request to the registered handler's channel.
3. Track the request in `pending` (same as built-in routing) so responses flow back to the original caller.
4. The handler sends response frames (Item*, Done/Error) back through its channel, which the kernel routes via `parent_id`.

### Sigcall Management Syscalls

The router handles `sigcall:*` requests inline (not routed to a subsystem):

| Syscall | Status | Description |
|---|---|---|
| `sigcall:list` | Implemented | Lists all registered handlers (streams Item frames) |
| `sigcall:register` | API only | Must be called via `SigcallRegistry` directly |
| `sigcall:unregister` | API only | Must be called via `SigcallRegistry` directly |

`sigcall:register` and `sigcall:unregister` are not yet functional as frame-based operations because they require passing a channel sender, which can't be carried in a frame. Registration and unregistration are done via the `SigcallRegistry` API before or during runtime.

### Ownership and Cleanup

- Each registration has an `owner` string (process ID, connection ID, subsystem name).
- Only the owner can unregister their handler.
- `unregister_all(owner)` is called on process exit / connection close to prevent orphaned registrations.
- First registration wins — attempting to register an already-taken name returns an error.

---

## Backpressure

Response streams can overwhelm slow consumers. The kernel implements watermark-based backpressure on response delivery (from monk-os StreamController and Abbot's dispatcher).

### StreamController

`StreamController` wraps an `mpsc::Sender<Frame>` and tracks per-stream buffer depth (keyed by `parent_id`). Subscribers are automatically wrapped when created via `kernel.subscribe()`.

```rust
pub struct BackpressureConfig {
    pub high_watermark: usize,      // default: 1000
    pub low_watermark: usize,       // default: 100
    pub stall_timeout: Duration,    // default: 5s
}

pub enum SendOutcome {
    Delivered,  // Frame was sent successfully
    Stalled,    // Consumer didn't drain within stall_timeout
    Closed,     // Channel closed — subscriber is gone
}
```

**Flow control per stream:**
- **Flowing** (buffered < `high_watermark`): Uses `try_send`. Falls back to blocking on channel-full.
- **Paused** (buffered >= `high_watermark`): Uses `send` with `stall_timeout`. Returns `Stalled` on timeout.
- **Terminal frames**: Always attempted, then stream tracking is cleaned up regardless of outcome.

**Where applied:**
- Between the router and external subscribers (gateway connections).
- Not applied between kernel and built-in subsystems (trusted, co-located code).

### Configuration

```rust
// Default backpressure
let kernel = Kernel::new();

// Custom backpressure
let kernel = Kernel::with_backpressure(BackpressureConfig {
    high_watermark: 500,
    low_watermark: 50,
    stall_timeout: Duration::from_secs(10),
});
```

---

## Cancellation

Cancellation is cooperative and propagated via `tokio_util::sync::CancellationToken`.

### Flow

1. Caller sends a `Cancel` frame with `parent_id` set to the target request's `id`.
2. Router looks up the target in `active` to find which subsystem is handling it.
3. Router triggers the `CancellationToken` associated with the request.
4. Router forwards the Cancel frame to the subsystem.
5. Router cleans up `pending` and `active` entries.

For `register_syscall` handlers, the subsystem receive loop maintains its own token map. When a Cancel frame arrives, it removes the token and calls `cancel()`. The handler observes cancellation via `cancel.is_cancelled()` or `cancel.cancelled().await`.

### In Syscall Handlers

```rust
async fn dispatch(
    &self,
    frame: &Frame,
    tx: &FrameSender,
    _caller: &Caller,
    cancel: CancellationToken,
) -> Result<(), Box<dyn ErrorCode + Send>> {
    for item in items {
        if cancel.is_cancelled() {
            return Err(Box::new(KernelError::cancelled()));
        }
        tx.send_item(frame, &item).await;
    }
    tx.send_done(frame).await;
    Ok(())
}
```

---

## Subscriber

External code (gateways, test harnesses) receives response frames via `kernel.subscribe()`, which returns a raw `mpsc::Receiver<Frame>`. Internally, each subscriber is wrapped in a `StreamController` for backpressure.

```rust
let mut rx = kernel.subscribe();
while let Some(frame) = rx.recv().await {
    // handle frame
}
```

Subscribers see **all** response frames that flow through the kernel (fan-out from `pending`). A subscriber is created via `kernel.subscribe()` before `kernel.start()`.

For room-scoped or connection-scoped delivery, the gateway layer filters frames by `parent_id` correlation or `trace` metadata — the kernel itself does not filter.

### Future: Subscriber Type

A dedicated `Subscriber` wrapper may be added to provide:
- Filtering by `parent_id` or syscall prefix (connection-scoped delivery)
- Typed deserialization helpers (`recv_as::<T>()`)
- Backpressure acknowledgment signals back to the `StreamController`
- Automatic reconnect/resubscribe on channel close

---

## Module Layout

```
rust-kernel/
├── Cargo.toml
├── DESIGN.md
├── src/
│   ├── lib.rs              — Crate root, re-exports
│   ├── frame.rs            — Frame, Status, Data, ErrorCode, to_data()
│   ├── kernel.rs           — Kernel builder: register(), subscribe(), start()
│   ├── router.rs           — Router runtime: event loop, route_request/response/cancel
│   ├── pipe.rs             — pipe(), PipeEnd, Caller, CallStream
│   ├── syscall.rs          — Syscall trait
│   ├── sender.rs           — FrameSender helper
│   ├── sigcall.rs          — SigcallRegistry
│   ├── backpressure.rs     — StreamController, BackpressureConfig, SendOutcome
│   └── error.rs            — KernelError, PipeError, SigcallError
```

---

## Dependencies

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime, mpsc channels, select!, JoinHandle |
| `tokio-util` | CancellationToken |
| `uuid` | Frame IDs (v4 or v7) |
| `serde` | Serialize/Deserialize for Frame, Data |
| `serde_json` | Value type for data payloads and trace metadata |
| `thiserror` | Error type derivation |
| `async-trait` | Syscall trait (until async fn in traits stabilizes fully) |

**Notably absent:**
- No `prost` / protobuf — serialization is the boundary's problem (see `rust-frames`).
- No `axum` / `hyper` / `tungstenite` — transport is the gateway's problem.
- No `sqlx` / `sqlite` — persistence is a subsystem's problem.

---

## Design Principles

1. **Message-pure**: Frames are Rust structs. No serialization inside the kernel. Channels carry owned values.

2. **Single event loop**: One inbound channel, one routing task. Simple, no deadlocks, easy to reason about.

3. **Prefix routing**: `syscall.split_once(':')` gives the prefix. O(1) HashMap lookup. Two-level dispatch: kernel routes by prefix, subsystem routes by verb.

4. **Handlers never touch channels directly**: Syscall trait implementors use `FrameSender` helpers. PipeEnd users use `Caller`/`CallStream`. The kernel owns all routing.

5. **Lazy allocation**: Pipe dispatchers only spawn when `call()` is first used. Most subsystems never need outbound calls.

6. **Zero globals**: Everything is passed explicitly (Arc, channel clones, function arguments). No OnceLock, no lazy_static.

7. **Transport-agnostic**: The kernel has no opinion about WebSocket, HTTP, TCP, or protobuf. Gateways are consumers that translate between wire protocols and Frame structs.

8. **Sigcalls for extensibility**: External handlers register at runtime. The kernel routes to them transparently. Cleanup on disconnect prevents orphaned registrations.

---

## Relationship to rust-frames

The `rust-frames` crate handles **wire encoding** (protobuf ↔ Frame). This crate handles **in-memory routing**. They share the same conceptual Frame shape but are independent:

- `rust-frames::Frame` uses `String` for IDs and `serde_json::Value` for data (wire-friendly).
- `rust-kernel::Frame` uses `Uuid` for IDs and `HashMap<String, Value>` for data (routing-friendly).
- Gateway code converts between the two at the WebSocket/HTTP boundary.

They can optionally share a dependency, or consumers can write their own conversion. The kernel crate does not depend on the frames crate.

---

## Prior Art Reference

### From monk-os-kernel (TypeScript)

**Sigcall registry**: Userspace processes register handlers via `sigcall:register`. Dispatcher checks registry for unknown syscalls and routes via `postMessage`. Responses stream back through `PendingSigcall` tracker with queue + promise resolver. Cleanup via `sigcallRegistry.unregisterAll(pid)` on process exit.

**StreamController backpressure**: High watermark (1000), low watermark (100), ping interval (100ms), stall timeout (5000ms). Pauses producer, resumes on consumer ack, cancels on stall.

**Handle interface**: Unified `exec(Message) → AsyncIterable<Response>` for files, sockets, pipes, ports, channels. Everything speaks the same frame protocol.

**Process model**: UUID process IDs, reference-counted handles, virtual processes for multi-tenancy.

### From prior (Rust)

**Pipe abstraction**: `pipe(capacity) → (PipeEnd, PipeEnd)`. Bidirectional crossed mpsc channels. Lazy dispatcher spawns only on first `call()`.

**Caller / CallStream**: `Caller` is cloneable, `CallStream` collects correlated responses. Pending map tracks `request_id → unbounded_sender`. Dispatcher routes by `parent_id`.

**Kernel router**: Single inbound channel. `PendingRoutes` (request → response destinations) and `ActiveRequests` (request → subsystem channel for cancel forwarding). Fan-out responses to all subscribers.

**FrameSender helpers**: `finish()`, `finish_item()`, `finish_items()` — pattern-matched on Result to send done/error. Used by VFS, EMS, Exec subsystems.

**Subsystem patterns**:
- VFS: `spawn_blocking` for file I/O
- EMS: Worker pool (8 workers) pulling from shared mpsc
- Room: Per-room workers with `tokio::select!` loop
- Exec: Process execution with stdout/stderr streaming and timeout

**ErrorCode trait**: `error_code() → &'static str`, `retryable() → bool`. Implemented by VfsError, EmsError, RoomError, ExecError.

**Code hygiene**: No `.unwrap()`, no globals, no dead code. Clippy pedantic. Ratchet budgets on `let _ =` and `.ok()`.

### From abbot (Rust)

**Syscall trait**: `name() → &'static str`, `execute(ctx, data, tx)`. SyscallContext carries call_id, actor, deadline, cancel token, cwd.

**KernelDispatcher**: HashMap of registered `Arc<dyn Syscall>`. Dispatch spawns exec task + pump task. Pump applies backpressure (high/low watermarks + stall timeout).

**Lane-based serialization**: `Immediate`, `Need`, `Room` lanes. Prevents deadlock (long-poll `need:lease` can't block `need:enqueue`).

**SigcallHub**: Manages turn streams keyed by `(room, thread_id)`. Dual delivery: broadcast to all observers + point-to-point to specific turn stream. Frame tagging with room metadata in trace. Audit persistence before delivery. Replace semantics on reconnect.

**KernelError**: Structured error with `code`, `message`, `help`, `detail`, `retryable`. Factory methods: `invalid_args()`, `forbidden()`, `not_found()`, `io()`, `timeout()`, `cancelled()`, `internal()`.

**Actor-based permissions**: `ctx.can_mutate()` checks actor prefix (`head/*` and `mind/*` can mutate; `hand/*` cannot).

### From gauntlet-week-1 (Rust)

**Outcome enum**: `Broadcast`, `BroadcastExcludeSender`, `Reply`, `ReplyStream { items, done }`, `ReplyAndBroadcast`, `ReplyStreamAndBroadcast`. Handlers return Outcome; dispatch layer applies it.

**Ephemeral frames**: Cursor and drag events skip persistence. Checked by prefix/syscall name.

**Broadcast mechanics**: Reply to sender includes `parent_id` for correlation. Peer broadcast strips `parent_id` (they didn't request it).

**protobuf wire format**: Binary encoding via Prost 0.13. Recursive `serde_json::Value ↔ prost_types::Value` conversion. Extracted to `rust-frames` crate.
