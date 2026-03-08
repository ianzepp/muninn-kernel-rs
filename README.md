# muninn-kernel

Transport-agnostic microkernel for routing in-memory frames between Rust subsystems.

`muninn-kernel` is the runtime half of the Muninn messaging stack:

- `muninn-kernel` handles in-memory routing, cancellation, and subscriber backpressure.
- `muninn-frames` handles wire encoding and decoding at transport boundaries.

The kernel never serializes frames. It routes native Rust structs over `tokio::sync::mpsc` channels and leaves WebSocket, HTTP, TCP, protobuf, and JSON concerns to gateway code.

## Installation

```toml
[dependencies]
kernel = { package = "muninn-kernel", git = "https://github.com/ianzepp/muninn-kernel-rs.git", tag = "v0.1.0" }
```

If you prefer the package name in code:

```toml
[dependencies]
muninn-kernel = { git = "https://github.com/ianzepp/muninn-kernel-rs.git", tag = "v0.1.0" }
```

## Core Types

- `Kernel`: registration and startup entry point
- `Frame`: in-memory request/response envelope
- `Syscall`: trait for prefix-owned handlers
- `FrameSender`: response helper for handlers
- `PipeEnd`, `Caller`, `CallStream`: lower-level pipe primitives
- `Subscriber`: response receiver returned by `Kernel::subscribe()`

## Minimal Example

```rust
use std::sync::Arc;

use async_trait::async_trait;
use kernel::{Caller, ErrorCode, Frame, FrameSender, Kernel, Syscall};
use tokio_util::sync::CancellationToken;

struct Echo;

#[async_trait]
impl Syscall for Echo {
    fn prefix(&self) -> &'static str {
        "echo"
    }

    async fn dispatch(
        &self,
        frame: &Frame,
        tx: &FrameSender,
        _caller: &Caller,
        _cancel: CancellationToken,
    ) -> Result<(), Box<dyn ErrorCode + Send>> {
        tx.send_done(frame).await.map_err(|err| {
            Box::new(kernel::KernelError::internal(err.to_string())) as Box<dyn ErrorCode + Send>
        })?;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let mut kernel = Kernel::new();
    kernel.register_syscall(Arc::new(Echo));

    let mut subscriber = kernel.subscribe();
    let sender = kernel.sender();
    let _handle = kernel.start();

    sender.send(Frame::request("echo:ping")).await.unwrap();

    let response = subscriber.recv().await.unwrap();
    assert_eq!(response.status, kernel::Status::Done);
}
```

## Wire Boundary

`muninn-kernel::Frame` is optimized for routing:

- `Uuid` IDs
- `HashMap<String, serde_json::Value>` payloads

`muninn-frames::Frame` is optimized for wire transport:

- `String` IDs
- `serde_json::Value` payloads

Gateway code should convert between them at the boundary. A separate bridge crate is the right place for shared conversions if several projects need the same adapter layer.

## Cancellation and Backpressure

- Cancel requests produce an immediate terminal `Status::Cancel` for subscribers.
- `register_syscall()` handlers receive an out-of-band cancellation signal through `CancellationToken`.
- Subscriber delivery is flow-controlled per response stream with configurable high and low watermarks.
- `Kernel::subscribe()` returns a `Subscriber` wrapper that acknowledges consumed frames back to the stream controller.

## Status

This crate is intended for reuse as a GitHub dependency across Muninn client and server projects. The current API is small, but still early-stage; pin to a tag or revision rather than tracking a moving branch.

See [`DESIGN.md`](DESIGN.md) for architecture notes and subsystem patterns.
