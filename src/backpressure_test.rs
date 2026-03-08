use std::time::Duration;

use tokio::sync::mpsc;
use crate::backpressure::{BackpressureConfig, SendOutcome, StreamController};
use crate::frame::{Frame, Status};

fn small_config() -> BackpressureConfig {
    BackpressureConfig {
        high_watermark: 3,
        low_watermark: 1,
        stall_timeout: Duration::from_millis(100),
    }
}

#[tokio::test]
async fn flowing_delivers_immediately() {
    let (tx, mut rx) = mpsc::channel(16);
    let ctrl = StreamController::new(tx, small_config());

    let req = Frame::request("test:op");
    let item = req.item(Default::default());

    let outcome = ctrl.send(&item).await;
    assert_eq!(outcome, SendOutcome::Delivered);

    let received = rx.recv().await.unwrap();
    assert_eq!(received.status, Status::Item);
}

#[tokio::test]
async fn tracks_buffered_count() {
    let (tx, _rx) = mpsc::channel(16);
    let ctrl = StreamController::new(tx, small_config());

    let req = Frame::request("test:op");
    let stream_id = req.id;

    for _ in 0..3 {
        let item = req.item(Default::default());
        ctrl.send(&item).await;
    }

    assert_eq!(ctrl.buffered(stream_id), 3);
}

#[tokio::test]
async fn terminal_frame_cleans_up_stream() {
    let (tx, _rx) = mpsc::channel(16);
    let ctrl = StreamController::new(tx, small_config());

    let req = Frame::request("test:op");
    let stream_id = req.id;

    // Send some items
    for _ in 0..2 {
        ctrl.send(&req.item(Default::default())).await;
    }
    assert_eq!(ctrl.buffered(stream_id), 2);

    // Send terminal
    let done = req.done();
    let outcome = ctrl.send(&done).await;
    assert_eq!(outcome, SendOutcome::Delivered);
    assert_eq!(ctrl.buffered(stream_id), 0);
}

#[tokio::test]
async fn closed_channel_returns_closed() {
    let (tx, rx) = mpsc::channel(16);
    let ctrl = StreamController::new(tx, small_config());
    drop(rx);

    let req = Frame::request("test:op");
    let item = req.item(Default::default());

    let outcome = ctrl.send(&item).await;
    assert_eq!(outcome, SendOutcome::Closed);
}

#[tokio::test]
async fn stall_timeout_returns_stalled() {
    // Channel capacity 1, high watermark 1 → immediately paused after first send
    let (tx, _rx) = mpsc::channel(1);
    let config = BackpressureConfig {
        high_watermark: 1,
        low_watermark: 0,
        stall_timeout: Duration::from_millis(50),
    };
    let ctrl = StreamController::new(tx, config);

    let req = Frame::request("test:op");

    // First item fills the channel
    let outcome = ctrl.send(&req.item(Default::default())).await;
    assert_eq!(outcome, SendOutcome::Delivered);

    // Second item: channel full + above watermark → blocking send → stall timeout
    let outcome = ctrl.send(&req.item(Default::default())).await;
    assert_eq!(outcome, SendOutcome::Stalled);
}

#[tokio::test]
async fn ack_stream_resets_count() {
    let (tx, _rx) = mpsc::channel(16);
    let ctrl = StreamController::new(tx, small_config());

    let req = Frame::request("test:op");
    let stream_id = req.id;

    for _ in 0..3 {
        ctrl.send(&req.item(Default::default())).await;
    }
    assert_eq!(ctrl.buffered(stream_id), 3);

    ctrl.ack_stream(stream_id);
    assert_eq!(ctrl.buffered(stream_id), 0);
}

#[tokio::test]
async fn frames_without_parent_id_always_deliver() {
    let (tx, _rx) = mpsc::channel(16);
    let ctrl = StreamController::new(tx, small_config());

    // A request frame has no parent_id
    let req = Frame::request("test:op");
    let outcome = ctrl.send(&req).await;
    assert_eq!(outcome, SendOutcome::Delivered);
}

#[tokio::test]
async fn high_watermark_triggers_backpressure() {
    // Use a large enough channel that the blocking send succeeds,
    // but verify we enter the paused path
    let (tx, mut rx) = mpsc::channel(16);
    let config = BackpressureConfig {
        high_watermark: 2,
        low_watermark: 1,
        stall_timeout: Duration::from_millis(100),
    };
    let ctrl = StreamController::new(tx, config);

    let req = Frame::request("test:op");

    // Send 2 items → at watermark
    for _ in 0..2 {
        let outcome = ctrl.send(&req.item(Default::default())).await;
        assert_eq!(outcome, SendOutcome::Delivered);
    }
    assert_eq!(ctrl.buffered(req.id), 2);

    // Third item triggers paused path but still delivers (channel has capacity)
    let outcome = ctrl.send(&req.item(Default::default())).await;
    assert_eq!(outcome, SendOutcome::Delivered);
    assert_eq!(ctrl.buffered(req.id), 3);

    // Drain to confirm all arrived
    for _ in 0..3 {
        rx.recv().await.unwrap();
    }
}

#[tokio::test]
async fn multiple_streams_tracked_independently() {
    let (tx, _rx) = mpsc::channel(32);
    let ctrl = StreamController::new(tx, small_config());

    let req_a = Frame::request("test:a");
    let req_b = Frame::request("test:b");

    for _ in 0..2 {
        ctrl.send(&req_a.item(Default::default())).await;
    }
    ctrl.send(&req_b.item(Default::default())).await;

    assert_eq!(ctrl.buffered(req_a.id), 2);
    assert_eq!(ctrl.buffered(req_b.id), 1);

    // Terminal on A doesn't affect B
    ctrl.send(&req_a.done()).await;
    assert_eq!(ctrl.buffered(req_a.id), 0);
    assert_eq!(ctrl.buffered(req_b.id), 1);
}

#[tokio::test]
async fn default_config_values() {
    let config = BackpressureConfig::default();
    assert_eq!(config.high_watermark, 1000);
    assert_eq!(config.low_watermark, 100);
    assert_eq!(config.stall_timeout, Duration::from_secs(5));
}

#[tokio::test]
async fn clone_shares_state() {
    let (tx, _rx) = mpsc::channel(16);
    let ctrl = StreamController::new(tx, small_config());
    let ctrl2 = ctrl.clone();

    let req = Frame::request("test:op");
    ctrl.send(&req.item(Default::default())).await;

    // Clone sees the same count
    assert_eq!(ctrl2.buffered(req.id), 1);
}
