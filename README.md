# muninn-kernel

Transport-agnostic microkernel for routing in-memory frames between Rust subsystems.

`muninn-kernel` is the runtime half of the Muninn messaging stack:

- **`muninn-kernel`** — in-memory routing, cancellation, backpressure, handler registration
- **`muninn-frames`** — wire encoding/decoding at transport boundaries

The kernel never serializes frames. It routes native Rust structs over `tokio::sync::mpsc` channels and leaves WebSocket, HTTP, TCP, protobuf, and JSON concerns entirely to gateway code.

## Installation

Add to your `Cargo.toml` as a git dependency:

```toml
[dependencies]
kernel = { package = "muninn-kernel", git = "https://github.com/ianzepp/muninn-kernel.git" }
```

Or, if you prefer to use the full package name in your `use` statements:

```toml
[dependencies]
muninn-kernel = { git = "https://github.com/ianzepp/muninn-kernel.git" }
```

Pin to a specific tag or commit rather than tracking `main`:

```toml
[dependencies]
kernel = { package = "muninn-kernel", git = "https://github.com/ianzepp/muninn-kernel.git", tag = "v0.1.0" }
```

## Architecture Overview

```
Callers
  │
  ▼ Frame::request("prefix:verb")
Kernel (builder)
  │  register_syscall(), register(), subscribe(), sender()
  │
  ▼ start()
Router (single async task)
  ├──▶ Syscall handlers  (registered by prefix, managed by kernel)
  ├──▶ Raw PipeEnds      (registered by prefix, managed by caller)
  ├──▶ SigcallRegistry   (dynamic handler registration at runtime)
  └──▶ Subscribers       (fan-out response delivery with backpressure)
```

**One event loop, no locks.** The router runs on a single task with plain `HashMap` state. This makes it easy to reason about and eliminates deadlocks.

**Prefix-based dispatch.** The syscall string `"prefix:verb"` is split at the first colon. The router does an O(1) lookup by prefix to find the right handler.

**Message-pure.** Frames are Rust structs moved through channels. Serialization happens only at the transport boundary, not inside the kernel.

## Core Concepts

### Frame

`Frame` is the universal in-memory envelope for every request and response:

```rust
pub struct Frame {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub ts: i64,
    pub from: Option<String>,
    pub syscall: String,
    pub status: Status,
    pub trace: serde_json::Value,
    pub data: Data,  // HashMap<String, serde_json::Value>
}
```

**Status lifecycle:**

```
Request  →  Item* / Bulk*  →  Done | Error | Cancel
```

| Status | Meaning |
|---|---|
| `Request` | Initial request from a caller |
| `Item` | Intermediate streaming result (non-terminal) |
| `Bulk` | Intermediate streaming batch (non-terminal) |
| `Done` | Successful terminal response |
| `Error` | Error terminal response |
| `Cancel` | Cancellation signal |

**Constructing frames:**

```rust
// Create a request
let req = Frame::request("vfs:read");
let req = Frame::request_with("vfs:read", data);

// Build response frames from a request (sets parent_id automatically)
let item  = req.item(data);
let bulk  = req.bulk(data);
let done  = req.done();
let done  = req.done_with(data);
let error = req.error("something went wrong");
let cancel = req.cancel();

// Serialize a struct directly into frame data
let item = req.item_from(&my_struct)?;
let done = req.done_from(&my_struct)?;

// Builder methods
let req = Frame::request("vfs:read")
    .with_from("user-42")
    .with_trace(json!({ "room": "abc" }))
    .with_field("path", "/home/user/file.txt")?;
```

### Kernel (Builder)

`Kernel` is the entry point for configuration and registration before the router starts:

```rust
let mut kernel = Kernel::new();

// Register a typed handler
kernel.register_syscall(Arc::new(MyHandler));

// Register a raw subsystem and get a PipeEnd
let pipe_end = kernel.register("prefix");

// Create a subscriber for response fan-out
let mut subscriber = kernel.subscribe();

// Get a sender to inject frames into the kernel
let sender = kernel.sender();

// Start the router (consumes the Kernel, spawns a task)
let _handle = kernel.start();
```

Custom backpressure settings:

```rust
let mut kernel = Kernel::with_backpressure(BackpressureConfig {
    high_watermark: 2000,
    low_watermark: 200,
    stall_timeout: Duration::from_secs(10),
});
```

### Syscall Trait

Implement `Syscall` to handle all requests under a prefix:

```rust
use async_trait::async_trait;
use kernel::{Caller, ErrorCode, Frame, FrameSender, Syscall};
use tokio_util::sync::CancellationToken;

struct VfsHandler;

#[async_trait]
impl Syscall for VfsHandler {
    fn prefix(&self) -> &'static str {
        "vfs"
    }

    async fn dispatch(
        &self,
        frame: &Frame,
        tx: &FrameSender,
        caller: &Caller,
        cancel: CancellationToken,
    ) -> Result<(), Box<dyn ErrorCode + Send>> {
        match frame.verb() {
            "read"  => self.read(frame, tx, cancel).await,
            "write" => self.write(frame, tx, cancel).await,
            verb    => Err(Box::new(KernelError::not_found(format!("unknown verb: {verb}")))),
        }
    }
}
```

- `frame.prefix()` extracts the prefix (`"vfs"` from `"vfs:read"`)
- `frame.verb()` extracts the verb (`"read"` from `"vfs:read"`)
- Returning `Err` causes the kernel to automatically send an `Error` frame to subscribers
- Check `cancel.is_cancelled()` at async yield points for cooperative cancellation

### FrameSender

`FrameSender` is a helper passed to `Syscall::dispatch` for sending response frames:

```rust
// Send a single item then done
tx.send_item(frame, &my_struct).await?;
tx.send_done(frame).await?;

// Or use the finish helpers:

// Sends Item + Done on Ok, Error on Err
tx.finish_item(frame, result).await?;

// Sends multiple Items + Done on Ok, Error on Err
tx.finish_items(frame, result_vec).await?;

// Sends Done on Ok, Error on Err (for unit results)
tx.finish(frame, result).await?;

// Send raw frames
tx.send(frame.done()).await?;
tx.send_error(frame, "something went wrong").await?;
tx.send_error_from(frame, &my_error).await?;
```

### Subscriber

`Subscriber` receives all response frames fanned out by the router. Create one before calling `kernel.start()`:

```rust
let mut subscriber = kernel.subscribe();

// In a gateway loop:
while let Some(frame) = subscriber.recv().await {
    // frame.parent_id links it to the originating request
    forward_to_client(frame).await;
}
```

Each `recv()` call automatically acknowledges the frame to the backpressure controller, allowing the router to resume delivery.

### PipeEnd and Caller

For subsystems that need to make outbound calls to other subsystems, use the pipe primitives directly:

```rust
// Get a PipeEnd when registering a raw subsystem
let mut pipe = kernel.register("db");

// Receive inbound requests
while let Some(frame) = pipe.recv().await {
    // Make an outbound call to another subsystem
    let mut caller = pipe.caller();
    let mut stream = caller.call(Frame::request("cache:get")
        .with_field("key", &cache_key)?).await?;

    while let Some(response) = stream.recv().await {
        // handle streaming responses
    }

    // Or collect all responses at once
    let all = stream.collect().await;

    // Send a response back
    pipe.sender().send(frame.done()).await?;
}
```

`Caller` is cloneable and can be passed into spawned tasks.

### SigcallRegistry

The sigcall registry allows external handlers to register at runtime — useful for connections or plugins that register handlers dynamically:

```rust
let sigcalls = kernel.sigcalls();

// Register a handler channel for a specific syscall
let (tx, mut rx) = tokio::sync::mpsc::channel(256);
sigcalls.register("room:create", "conn-abc123", tx)?;

// Serve requests in a task
tokio::spawn(async move {
    while let Some(request) = rx.recv().await {
        let response = handle_room_create(&request).await;
        kernel_sender.send(response).await.ok();
    }
});

// On disconnect, remove all handlers for this owner
sigcalls.unregister_all("conn-abc123");
```

Ownership rules:
- First registration wins — a different owner cannot shadow an existing registration
- Only the registered owner can unregister a handler
- Names prefixed with `"sigcall:"` are reserved for kernel use

## Error Types

```rust
// KernelError — structured errors with code and retryable flag
KernelError::invalid_args("missing field: path")
KernelError::not_found("no route for prefix: vfs")
KernelError::forbidden("owner mismatch")
KernelError::cancelled()
KernelError::timeout("subscriber stalled")   // retryable = true
KernelError::internal("unexpected state")
KernelError::no_route("unknown prefix: xyz")

// PipeError — channel-level errors
PipeError::Closed
PipeError::SendFailed
PipeError::Serialization(serde_json::Error)

// SigcallError — registration errors
SigcallError::AlreadyRegistered { name, owner }
SigcallError::NotRegistered { name }
SigcallError::NotOwner { name, owner, caller }
SigcallError::Reserved { name }
```

Implement `ErrorCode` on your own error types to use `Frame::error_from()` and `FrameSender::send_error_from()`:

```rust
impl ErrorCode for MyError {
    fn code(&self) -> &'static str { "E_MY_ERROR" }
    fn retryable(&self) -> bool { false }
}
```

## Backpressure

Backpressure is applied per response stream, not globally. One slow subscriber does not block unrelated streams.

```rust
BackpressureConfig {
    high_watermark: 1000,              // pause delivery above this
    low_watermark: 100,                // resume delivery below this
    stall_timeout: Duration::from_secs(5),  // error on prolonged stall
}
```

Terminal frames (`Done`, `Error`, `Cancel`) are always delivered regardless of watermark state, and stream tracking is cleaned up immediately after.

## Cancellation

Send a `Cancel` frame to abort an in-flight request:

```rust
// The original request
let req = Frame::request("vfs:read").with_field("path", "/large/file")?;
sender.send(req.clone()).await?;

// Later, cancel it
sender.send(req.cancel()).await?;
```

The router:
1. Delivers a `Cancel` frame to all subscribers immediately
2. Fires the `CancellationToken` passed to the active `Syscall::dispatch` call
3. Cleans up tracking state for that request

Handlers must observe `cancel.is_cancelled()` at yield points — cancellation is cooperative, not preemptive.

## Gateway Pattern

Typical usage in a WebSocket gateway:

```rust
use std::sync::Arc;
use kernel::{Kernel, Frame};

#[tokio::main]
async fn main() {
    let mut kernel = Kernel::new();

    // Register subsystems
    kernel.register_syscall(Arc::new(VfsHandler));
    kernel.register_syscall(Arc::new(EmsHandler));
    kernel.register_syscall(Arc::new(RoomHandler));

    // Create a subscriber for outbound delivery
    let mut subscriber = kernel.subscribe();
    let sender = kernel.sender();

    // Start the kernel router
    let _handle = kernel.start();

    // In your WebSocket accept loop — spawn per connection:
    tokio::spawn(async move {
        // Receive frames from the transport, inject into kernel
        let incoming: Frame = receive_from_ws().await;
        sender.send(incoming).await.ok();
    });

    // Fan out responses to clients
    while let Some(frame) = subscriber.recv().await {
        deliver_to_client(frame).await;
    }
}
```

## Wire Boundary

`muninn-kernel::Frame` is optimized for routing:

- `Uuid` identifiers (efficient routing, not serialized inside kernel)
- `HashMap<String, serde_json::Value>` payloads (fast field access)

`muninn-frames::Frame` is optimized for transport:

- `String` identifiers
- `serde_json::Value` payloads

Convert between them at the gateway boundary. If multiple projects share the same conversion logic, a small bridge crate is the right place for it rather than coupling either library to the other.

## Module Reference

| Module | Purpose |
|---|---|
| `frame` | `Frame`, `Status`, `Data`, `ErrorCode`, `to_data()` |
| `kernel` | `Kernel` builder and registration API |
| `router` | Router event loop (internal, not public) |
| `pipe` | `PipeEnd`, `Caller`, `CallStream` |
| `syscall` | `Syscall` trait |
| `sender` | `FrameSender` response helpers |
| `backpressure` | `BackpressureConfig`, `StreamController`, `Subscriber`, `SendOutcome` |
| `sigcall` | `SigcallRegistry`, `SigcallError` |
| `error` | `KernelError`, `PipeError` |

## Status

The API is small and early-stage. Pin to a tag or revision rather than tracking a moving branch. See [`DESIGN.md`](DESIGN.md) for architecture notes and subsystem patterns.
