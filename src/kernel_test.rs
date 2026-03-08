use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::frame::{Frame, Status};
use crate::kernel::Kernel;
use crate::pipe::Caller;
use crate::sender::FrameSender;
use crate::syscall::Syscall;

/// A simple echo syscall: responds with a done frame for every request.
struct EchoSyscall;

#[async_trait]
impl Syscall for EchoSyscall {
    fn prefix(&self) -> &'static str {
        "echo"
    }

    async fn dispatch(
        &self,
        frame: &Frame,
        tx: &FrameSender,
        _caller: &Caller,
        _cancel: CancellationToken,
    ) {
        let _ = tx.send_done(frame).await;
    }
}

/// Streams N items then done.
struct StreamSyscall;

#[async_trait]
impl Syscall for StreamSyscall {
    fn prefix(&self) -> &'static str {
        "stream"
    }

    async fn dispatch(
        &self,
        frame: &Frame,
        tx: &FrameSender,
        _caller: &Caller,
        _cancel: CancellationToken,
    ) {
        for i in 0..3 {
            let mut data = crate::frame::Data::new();
            data.insert("index".into(), serde_json::Value::from(i));
            let _ = tx.send(frame.item(data)).await;
        }
        let _ = tx.send_done(frame).await;
    }
}

#[tokio::test]
async fn kernel_routes_request_to_registered_subsystem() {
    let mut kernel = Kernel::new();
    let mut sub_end = kernel.register("test");
    let mut rx = kernel.subscribe();
    let sender = kernel.sender();

    let _handle = kernel.start();

    // Send a request
    let req = Frame::request("test:ping");
    let req_id = req.id;
    sender.send(req).await.unwrap();

    // Subsystem receives it
    let received = sub_end.recv().await.unwrap();
    assert_eq!(received.id, req_id);
    assert_eq!(received.syscall, "test:ping");

    // Subsystem responds
    sub_end.sender().send(received.done()).await.unwrap();

    // Subscriber receives the response
    let response = rx.recv().await.unwrap();
    assert_eq!(response.parent_id, Some(req_id));
    assert_eq!(response.status, Status::Done);
}

#[tokio::test]
async fn kernel_returns_error_for_unknown_prefix() {
    let mut kernel = Kernel::new();
    let mut rx = kernel.subscribe();
    let sender = kernel.sender();

    let _handle = kernel.start();

    let req = Frame::request("unknown:op");
    sender.send(req).await.unwrap();

    let response = rx.recv().await.unwrap();
    assert_eq!(response.status, Status::Error);
    assert!(
        response
            .data
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("NO_ROUTE")
    );
}

#[tokio::test]
async fn kernel_routes_streaming_responses() {
    let mut kernel = Kernel::new();
    let mut sub_end = kernel.register("data");
    let mut rx = kernel.subscribe();
    let sender = kernel.sender();

    let _handle = kernel.start();

    let req = Frame::request("data:list");
    let req_id = req.id;
    sender.send(req).await.unwrap();

    let received = sub_end.recv().await.unwrap();

    // Send 3 items then done
    for _ in 0..3 {
        sub_end
            .sender()
            .send(received.item(Default::default()))
            .await
            .unwrap();
    }
    sub_end.sender().send(received.done()).await.unwrap();

    // Subscriber receives all 4 frames
    let mut items = 0;
    let mut done = false;
    for _ in 0..4 {
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.parent_id, Some(req_id));
        match frame.status {
            Status::Item => items += 1,
            Status::Done => done = true,
            _ => panic!("unexpected status: {:?}", frame.status),
        }
    }
    assert_eq!(items, 3);
    assert!(done);
}

#[tokio::test]
async fn kernel_register_syscall_dispatches_via_trait() {
    let mut kernel = Kernel::new();
    kernel.register_syscall(Arc::new(EchoSyscall));
    let mut rx = kernel.subscribe();
    let sender = kernel.sender();

    let _handle = kernel.start();

    let req = Frame::request("echo:hello");
    let req_id = req.id;
    sender.send(req).await.unwrap();

    let response = rx.recv().await.unwrap();
    assert_eq!(response.parent_id, Some(req_id));
    assert_eq!(response.status, Status::Done);
}

#[tokio::test]
async fn kernel_register_syscall_streams() {
    let mut kernel = Kernel::new();
    kernel.register_syscall(Arc::new(StreamSyscall));
    let mut rx = kernel.subscribe();
    let sender = kernel.sender();

    let _handle = kernel.start();

    let req = Frame::request("stream:data");
    let req_id = req.id;
    sender.send(req).await.unwrap();

    let mut frames = Vec::new();
    loop {
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.parent_id, Some(req_id));
        let terminal = frame.status.is_terminal();
        frames.push(frame);
        if terminal {
            break;
        }
    }

    assert_eq!(frames.len(), 4); // 3 items + 1 done
}

#[tokio::test]
async fn kernel_sigcall_routes_to_registered_handler() {
    let mut kernel = Kernel::new();
    let mut rx = kernel.subscribe();
    let sender = kernel.sender();

    // Register a sigcall handler
    let (handler_tx, mut handler_rx) = mpsc::channel::<Frame>(16);
    kernel
        .sigcalls()
        .register("custom:op", "plugin-1", handler_tx)
        .unwrap();

    let _handle = kernel.start();

    // Send a request for the sigcall
    let req = Frame::request("custom:op");
    let req_id = req.id;
    sender.send(req).await.unwrap();

    // Handler receives the request
    let received = handler_rx.recv().await.unwrap();
    assert_eq!(received.id, req_id);

    // Handler responds through the kernel
    sender.send(received.done()).await.unwrap();

    // Subscriber receives the response
    let response = rx.recv().await.unwrap();
    assert_eq!(response.parent_id, Some(req_id));
    assert_eq!(response.status, Status::Done);
}

#[tokio::test]
async fn kernel_sigcall_list() {
    let mut kernel = Kernel::new();
    let mut rx = kernel.subscribe();
    let sender = kernel.sender();

    let (handler_tx, _handler_rx) = mpsc::channel::<Frame>(16);
    kernel
        .sigcalls()
        .register("custom:a", "owner-1", handler_tx.clone())
        .unwrap();
    kernel
        .sigcalls()
        .register("custom:b", "owner-2", handler_tx)
        .unwrap();

    let _handle = kernel.start();

    let req = Frame::request("sigcall:list");
    sender.send(req).await.unwrap();

    // Collect responses: should be 2 items + 1 done
    let mut items = Vec::new();
    loop {
        let frame = rx.recv().await.unwrap();
        let terminal = frame.status.is_terminal();
        if frame.status == Status::Item {
            items.push(frame);
        }
        if terminal {
            break;
        }
    }

    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn kernel_cancel_cleans_up_routing_state() {
    let mut kernel = Kernel::new();
    let mut sub_end = kernel.register("slow");
    let mut rx = kernel.subscribe();
    let sender = kernel.sender();

    let _handle = kernel.start();

    // Send a request
    let req = Frame::request("slow:work");
    let req_id = req.id;
    sender.send(req).await.unwrap();

    // Subsystem receives it
    let _received = sub_end.recv().await.unwrap();

    // Cancel the request
    let cancel = Frame::request("slow:work").cancel();
    let mut cancel_frame = cancel;
    cancel_frame.parent_id = Some(req_id);
    sender.send(cancel_frame).await.unwrap();

    // Subsystem receives the cancel
    let cancel_received = sub_end.recv().await.unwrap();
    assert_eq!(cancel_received.status, Status::Cancel);

    // A subsequent response for this request should not reach subscribers
    // (pending was cleaned up). We verify by sending a new unrelated request
    // and checking that we receive its response, not a stale one.
    let req2 = Frame::request("slow:other");
    let req2_id = req2.id;
    sender.send(req2).await.unwrap();

    let received2 = sub_end.recv().await.unwrap();
    sub_end.sender().send(received2.done()).await.unwrap();

    let response = rx.recv().await.unwrap();
    assert_eq!(response.parent_id, Some(req2_id));
}

/// A syscall that blocks until cancelled, then records that cancellation happened.
struct CancellableSyscall {
    was_cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl Syscall for CancellableSyscall {
    fn prefix(&self) -> &'static str {
        "slow"
    }

    async fn dispatch(
        &self,
        frame: &Frame,
        tx: &FrameSender,
        _caller: &Caller,
        cancel: CancellationToken,
    ) {
        // Wait for cancellation
        cancel.cancelled().await;
        self.was_cancelled.store(true, Ordering::SeqCst);
        let _ = tx.send_error(frame, "cancelled").await;
    }
}

#[tokio::test]
async fn kernel_cancel_triggers_token_for_syscall_handler() {
    let was_cancelled = Arc::new(AtomicBool::new(false));

    let mut kernel = Kernel::new();
    kernel.register_syscall(Arc::new(CancellableSyscall {
        was_cancelled: Arc::clone(&was_cancelled),
    }));
    let _rx = kernel.subscribe();
    let sender = kernel.sender();

    let _handle = kernel.start();

    // Send a request that will block until cancelled
    let req = Frame::request("slow:work");
    let req_id = req.id;
    sender.send(req).await.unwrap();

    // Give the handler time to start
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!was_cancelled.load(Ordering::SeqCst));

    // Send cancel
    let mut cancel_frame = Frame::request("slow:work");
    cancel_frame.status = Status::Cancel;
    cancel_frame.parent_id = Some(req_id);
    sender.send(cancel_frame).await.unwrap();

    // Give the handler time to observe cancellation
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(was_cancelled.load(Ordering::SeqCst));
}
