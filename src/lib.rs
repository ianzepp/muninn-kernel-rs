//! Transport-agnostic microkernel for in-memory frame routing.
//!
//! All communication uses [`Frame`] structs passed through `mpsc` channels.
//! No serialization occurs inside the kernel — that happens at I/O boundaries.
//!
//! # Architecture
//!
//! The kernel is a single async event loop that routes frames by syscall prefix.
//! Subsystems register to handle prefixes and communicate through bidirectional
//! pipes. External handlers can register dynamically via the sigcall registry.
//!
//! ```text
//! Syscall:  caller → kernel → built-in handler → kernel → caller
//! Sigcall:  caller → kernel → registered handler → kernel → caller
//! ```

pub mod backpressure;
pub mod error;
pub mod frame;
pub mod kernel;
pub mod pipe;
pub mod sender;
pub mod sigcall;
pub mod syscall;

pub use backpressure::{BackpressureConfig, SendOutcome, StreamController};
pub use error::{KernelError, PipeError, SigcallError};
pub use frame::{Data, ErrorCode, Frame, Status};
pub use kernel::Kernel;
pub use pipe::{CallStream, Caller, PipeEnd, pipe, pipe_default};
pub use sender::FrameSender;
pub use sigcall::SigcallRegistry;
pub use syscall::Syscall;
