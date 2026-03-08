//! Syscall trait — the handler contract for kernel subsystems.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::frame::Frame;
use crate::pipe::Caller;
use crate::sender::FrameSender;

/// Trait for kernel subsystem handlers.
///
/// Implementors handle all syscalls for a given prefix (e.g., `"vfs"`, `"ems"`).
/// The kernel routes request frames by prefix and calls `dispatch` on the
/// matching handler.
///
/// # Response Protocol
///
/// Handlers emit response frames via the provided [`FrameSender`]. Every
/// dispatch must produce exactly one terminal frame (`Done` or `Error`).
/// Streaming responses emit zero or more `Item`/`Bulk` frames before the
/// terminal.
///
/// # Cancellation
///
/// Handlers should check `cancel.is_cancelled()` at yield points and exit
/// early when cancelled. The kernel triggers the token when a `Cancel` frame
/// arrives for an in-flight request.
#[async_trait]
pub trait Syscall: Send + Sync {
    /// The syscall prefix this handler owns (e.g., `"vfs"`, `"ems"`, `"room"`).
    fn prefix(&self) -> &'static str;

    /// Handle a request frame.
    ///
    /// - `frame`: The inbound request (status = Request).
    /// - `tx`: Sender for emitting response frames (Item, Done, Error).
    /// - `caller`: For making outbound calls to other subsystems through the kernel.
    /// - `cancel`: Triggered when the caller cancels this request.
    async fn dispatch(
        &self,
        frame: &Frame,
        tx: &FrameSender,
        caller: &Caller,
        cancel: CancellationToken,
    );
}
