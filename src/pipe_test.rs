use crate::frame::{Frame, Status};
use crate::pipe::{pipe, pipe_default};

#[tokio::test]
async fn pipe_send_and_recv() {
    let (mut end_a, mut end_b) = pipe(16);

    let frame = Frame::request("test:ping");
    let id = frame.id;

    end_a.sender().send(frame).await.unwrap();
    let received = end_b.recv().await.unwrap();

    assert_eq!(received.id, id);
    assert_eq!(received.call, "test:ping");
}

#[tokio::test]
async fn pipe_bidirectional() {
    let (mut end_a, mut end_b) = pipe(16);

    let req = Frame::request("test:op");
    let req_id = req.id;
    end_a.sender().send(req).await.unwrap();

    let received = end_b.recv().await.unwrap();
    let resp = received.done();
    end_b.sender().send(resp).await.unwrap();

    let response = end_a.recv().await.unwrap();
    assert_eq!(response.parent_id, Some(req_id));
    assert_eq!(response.status, Status::Done);
}

#[tokio::test]
async fn caller_call_receives_correlated_responses() {
    let (mut end_a, mut end_b) = pipe(16);
    let caller = end_a.caller();

    let req = Frame::request("test:op");
    let req_id = req.id;

    let mut stream = caller.call(req).await.unwrap();

    // Simulate subsystem responding
    let received = end_b.recv().await.unwrap();
    assert_eq!(received.id, req_id);

    let item = received.item(Default::default());
    end_b.sender().send(item).await.unwrap();

    let done = received.done();
    end_b.sender().send(done).await.unwrap();

    // Caller receives correlated responses
    let resp1 = stream.recv().await.unwrap();
    assert_eq!(resp1.status, Status::Item);
    assert_eq!(resp1.parent_id, Some(req_id));

    let resp2 = stream.recv().await.unwrap();
    assert_eq!(resp2.status, Status::Done);
}

#[tokio::test]
async fn caller_call_collect_gathers_until_terminal() {
    let (mut end_a, mut end_b) = pipe(16);
    let caller = end_a.caller();

    let req = Frame::request("test:op");
    let stream = caller.call(req).await.unwrap();

    let received = end_b.recv().await.unwrap();
    end_b
        .sender()
        .send(received.item(Default::default()))
        .await
        .unwrap();
    end_b
        .sender()
        .send(received.item(Default::default()))
        .await
        .unwrap();
    end_b.sender().send(received.done()).await.unwrap();

    let frames = stream.collect().await;
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].status, Status::Item);
    assert_eq!(frames[1].status, Status::Item);
    assert_eq!(frames[2].status, Status::Done);
}

#[tokio::test]
async fn caller_multiple_concurrent_calls() {
    let (mut end_a, mut end_b) = pipe(16);
    let caller = end_a.caller();

    let req1 = Frame::request("test:first");
    let req2 = Frame::request("test:second");
    let id1 = req1.id;
    let id2 = req2.id;

    let mut stream1 = caller.call(req1).await.unwrap();
    let mut stream2 = caller.call(req2).await.unwrap();

    // Subsystem receives both
    let r1 = end_b.recv().await.unwrap();
    let r2 = end_b.recv().await.unwrap();

    // Respond out of order (second first)
    end_b.sender().send(r2.done()).await.unwrap();
    end_b.sender().send(r1.done()).await.unwrap();

    // Each stream gets its own response
    let resp2 = stream2.recv().await.unwrap();
    assert_eq!(resp2.parent_id, Some(id2));

    let resp1 = stream1.recv().await.unwrap();
    assert_eq!(resp1.parent_id, Some(id1));
}

#[tokio::test]
async fn caller_send_fire_and_forget() {
    let (mut end_a, mut end_b) = pipe(16);
    let caller = end_a.caller();

    let frame = Frame::request("test:broadcast");
    caller.send(frame).await.unwrap();

    let received = end_b.recv().await.unwrap();
    assert_eq!(received.call, "test:broadcast");
}

#[tokio::test]
async fn unmatched_frames_go_to_recv() {
    let (mut end_a, mut end_b) = pipe(16);
    let _caller = end_a.caller(); // Activate dispatcher

    // Send a request (no parent_id, so it won't match any pending call)
    let frame = Frame::request("test:inbound");
    end_b.sender().send(frame).await.unwrap();

    // Should be available via recv()
    let received = end_a.recv().await.unwrap();
    assert_eq!(received.call, "test:inbound");
}

#[tokio::test]
async fn call_stream_drop_cleans_up_pending() {
    let (mut end_a, _end_b) = pipe(16);
    let caller = end_a.caller();

    let req = Frame::request("test:op");
    let id = req.id;
    let stream = caller.call(req).await.unwrap();

    // Verify pending entry exists
    {
        let guard = caller.pending.lock().unwrap();
        assert!(guard.contains_key(&id));
    }

    // Drop the stream
    drop(stream);

    // Pending entry should be cleaned up
    {
        let guard = caller.pending.lock().unwrap();
        assert!(!guard.contains_key(&id));
    }
}

#[tokio::test]
async fn pipe_default_works() {
    let (mut end_a, mut end_b) = pipe_default();

    let frame = Frame::request("test:default");
    end_a.sender().send(frame).await.unwrap();

    let received = end_b.recv().await.unwrap();
    assert_eq!(received.call, "test:default");
}
