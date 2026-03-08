use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::frame::{ErrorCode, Frame, Status};
use crate::sender::FrameSender;

fn setup() -> (FrameSender, mpsc::Receiver<Frame>) {
    let (tx, rx) = mpsc::channel(64);
    (FrameSender::new(tx), rx)
}

#[derive(Serialize)]
struct TestItem {
    name: String,
    count: u32,
}

struct FailingItem;

impl Serialize for FailingItem {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("boom"))
    }
}

#[derive(Debug)]
struct TestError;
impl std::fmt::Display for TestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "something broke")
    }
}
impl ErrorCode for TestError {
    fn error_code(&self) -> &'static str {
        "E_TEST"
    }
}

#[tokio::test]
async fn send_item_serializes_struct_to_data() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");
    let item = TestItem {
        name: "foo".into(),
        count: 3,
    };

    sender.send_item(&req, &item).await.unwrap();
    let frame = rx.recv().await.unwrap();

    assert_eq!(frame.status, Status::Item);
    assert_eq!(frame.parent_id, Some(req.id));
    assert_eq!(frame.data.get("name").and_then(Value::as_str), Some("foo"));
    assert_eq!(frame.data.get("count").and_then(Value::as_u64), Some(3));
}

#[tokio::test]
async fn send_done_emits_terminal() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");

    sender.send_done(&req).await.unwrap();
    let frame = rx.recv().await.unwrap();

    assert_eq!(frame.status, Status::Done);
    assert!(frame.data.is_empty());
}

#[tokio::test]
async fn send_error_emits_structured_error() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");

    sender.send_error(&req, "bad input").await.unwrap();
    let frame = rx.recv().await.unwrap();

    assert_eq!(frame.status, Status::Error);
    assert_eq!(
        frame.data.get("message").and_then(Value::as_str),
        Some("bad input")
    );
}

#[tokio::test]
async fn send_error_from_uses_error_code() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");

    sender.send_error_from(&req, &TestError).await.unwrap();
    let frame = rx.recv().await.unwrap();

    assert_eq!(frame.status, Status::Error);
    assert_eq!(
        frame.data.get("code").and_then(Value::as_str),
        Some("E_TEST")
    );
    assert_eq!(
        frame.data.get("message").and_then(Value::as_str),
        Some("something broke")
    );
}

#[tokio::test]
async fn finish_item_ok_sends_item_then_done() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");
    let result: Result<TestItem, TestError> = Ok(TestItem {
        name: "bar".into(),
        count: 7,
    });

    sender.finish_item(&req, result).await.unwrap();

    let item = rx.recv().await.unwrap();
    assert_eq!(item.status, Status::Item);
    assert_eq!(item.data.get("name").and_then(Value::as_str), Some("bar"));

    let done = rx.recv().await.unwrap();
    assert_eq!(done.status, Status::Done);
}

#[tokio::test]
async fn finish_item_err_sends_error() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");
    let result: Result<TestItem, TestError> = Err(TestError);

    sender.finish_item(&req, result).await.unwrap();

    let err = rx.recv().await.unwrap();
    assert_eq!(err.status, Status::Error);
    assert_eq!(err.data.get("code").and_then(Value::as_str), Some("E_TEST"));
}

#[tokio::test]
async fn finish_items_ok_sends_all_then_done() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");
    let items = vec![
        TestItem {
            name: "a".into(),
            count: 1,
        },
        TestItem {
            name: "b".into(),
            count: 2,
        },
        TestItem {
            name: "c".into(),
            count: 3,
        },
    ];
    let result: Result<Vec<TestItem>, TestError> = Ok(items);

    sender.finish_items(&req, result).await.unwrap();

    for expected in ["a", "b", "c"] {
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.status, Status::Item);
        assert_eq!(
            frame.data.get("name").and_then(Value::as_str),
            Some(expected)
        );
    }

    let done = rx.recv().await.unwrap();
    assert_eq!(done.status, Status::Done);
}

#[tokio::test]
async fn finish_items_err_sends_error() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");
    let result: Result<Vec<TestItem>, TestError> = Err(TestError);

    sender.finish_items(&req, result).await.unwrap();

    let err = rx.recv().await.unwrap();
    assert_eq!(err.status, Status::Error);
}

#[tokio::test]
async fn finish_ok_sends_done() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");
    let result: Result<(), TestError> = Ok(());

    sender.finish(&req, result).await.unwrap();

    let done = rx.recv().await.unwrap();
    assert_eq!(done.status, Status::Done);
}

#[tokio::test]
async fn finish_err_sends_error() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");
    let result: Result<(), TestError> = Err(TestError);

    sender.finish(&req, result).await.unwrap();

    let err = rx.recv().await.unwrap();
    assert_eq!(err.status, Status::Error);
}

#[tokio::test]
async fn to_data_wraps_non_object_under_value_key() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");

    sender.send_item(&req, &42u32).await.unwrap();
    let frame = rx.recv().await.unwrap();

    assert_eq!(frame.data.get("value").and_then(Value::as_u64), Some(42));
}

#[tokio::test]
async fn send_item_returns_serialize_error() {
    let (sender, mut rx) = setup();
    let req = Frame::request("test:op");

    let err = sender.send_item(&req, &FailingItem).await.unwrap_err();
    assert_eq!(err.to_string(), "serialization failed: boom");
    assert!(rx.try_recv().is_err());
}
