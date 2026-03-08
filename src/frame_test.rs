use super::*;

#[test]
fn request_creates_frame_with_correct_status() {
    let f = Frame::request("vfs:read");
    assert_eq!(f.status, Status::Request);
    assert_eq!(f.syscall, "vfs:read");
    assert!(f.parent_id.is_none());
    assert!(f.from.is_none());
    assert!(f.data.is_empty());
}

#[test]
fn request_with_populates_data() {
    let mut data = HashMap::new();
    data.insert("path".into(), Value::String("/tmp/test".into()));
    let f = Frame::request_with("vfs:read", data);
    assert_eq!(
        f.data.get("path").and_then(Value::as_str),
        Some("/tmp/test")
    );
}

#[test]
fn item_response_correlates_to_request() {
    let req = Frame::request("vfs:read");
    let item = req.item(HashMap::new());
    assert_eq!(item.parent_id, Some(req.id));
    assert_eq!(item.status, Status::Item);
    assert_eq!(item.syscall, "vfs:read");
}

#[test]
fn bulk_response_correlates_to_request() {
    let req = Frame::request("ems:list");
    let bulk = req.bulk(HashMap::new());
    assert_eq!(bulk.parent_id, Some(req.id));
    assert_eq!(bulk.status, Status::Bulk);
}

#[test]
fn done_response_is_terminal() {
    let req = Frame::request("vfs:write");
    let done = req.done();
    assert_eq!(done.parent_id, Some(req.id));
    assert_eq!(done.status, Status::Done);
    assert!(done.data.is_empty());
    assert!(done.status.is_terminal());
}

#[test]
fn done_with_carries_data() {
    let req = Frame::request("ems:create");
    let mut data = HashMap::new();
    data.insert("id".into(), Value::String("abc".into()));
    let done = req.done_with(data);
    assert_eq!(done.data.get("id").and_then(Value::as_str), Some("abc"));
    assert!(done.status.is_terminal());
}

#[test]
fn error_response_has_structured_fields() {
    let req = Frame::request("vfs:read");
    let err = req.error("file not found");
    assert_eq!(err.status, Status::Error);
    assert_eq!(err.parent_id, Some(req.id));
    assert_eq!(
        err.data.get("code").and_then(Value::as_str),
        Some("E_INTERNAL")
    );
    assert_eq!(
        err.data.get("message").and_then(Value::as_str),
        Some("file not found")
    );
    assert_eq!(
        err.data.get("retryable").and_then(Value::as_bool),
        Some(false)
    );
}

#[test]
fn error_from_uses_error_code_trait() {
    #[derive(Debug)]
    struct TestError;
    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "test error")
        }
    }
    impl ErrorCode for TestError {
        fn error_code(&self) -> &'static str {
            "E_TEST"
        }
        fn retryable(&self) -> bool {
            true
        }
    }

    let req = Frame::request("test:op");
    let err = req.error_from(&TestError);
    assert_eq!(err.data.get("code").and_then(Value::as_str), Some("E_TEST"));
    assert_eq!(
        err.data.get("message").and_then(Value::as_str),
        Some("test error")
    );
    assert_eq!(
        err.data.get("retryable").and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn cancel_frame_is_terminal() {
    let req = Frame::request("ai:prompt");
    let cancel = req.cancel();
    assert_eq!(cancel.parent_id, Some(req.id));
    assert_eq!(cancel.status, Status::Cancel);
    assert!(cancel.status.is_terminal());
}

#[test]
fn prefix_extracts_namespace() {
    let f = Frame::request("vfs:read");
    assert_eq!(f.prefix(), "vfs");
}

#[test]
fn prefix_returns_full_syscall_when_no_colon() {
    let f = Frame::request("health");
    assert_eq!(f.prefix(), "health");
}

#[test]
fn verb_extracts_operation() {
    let f = Frame::request("vfs:read");
    assert_eq!(f.verb(), "read");
}

#[test]
fn verb_returns_empty_when_no_colon() {
    let f = Frame::request("health");
    assert_eq!(f.verb(), "");
}

#[test]
fn with_from_sets_sender() {
    let f = Frame::request("test:op").with_from("user-42");
    assert_eq!(f.from.as_deref(), Some("user-42"));
}

#[test]
fn with_trace_sets_metadata() {
    let trace = serde_json::json!({"room": "main"});
    let f = Frame::request("test:op").with_trace(trace.clone());
    assert_eq!(f.trace, Some(trace));
}

#[test]
fn with_data_adds_key_value() {
    let f = Frame::request("test:op").with_data("key", Value::Bool(true));
    assert_eq!(f.data.get("key").and_then(Value::as_bool), Some(true));
}

#[test]
fn status_terminal_checks() {
    assert!(!Status::Request.is_terminal());
    assert!(!Status::Item.is_terminal());
    assert!(!Status::Bulk.is_terminal());
    assert!(Status::Done.is_terminal());
    assert!(Status::Error.is_terminal());
    assert!(Status::Cancel.is_terminal());
}

#[test]
fn each_frame_gets_unique_id() {
    let a = Frame::request("test:a");
    let b = Frame::request("test:b");
    assert_ne!(a.id, b.id);
}

#[test]
fn response_inherits_trace_from_request() {
    let trace = serde_json::json!({"span_id": "abc"});
    let req = Frame::request("test:op").with_trace(trace.clone());
    let item = req.item(HashMap::new());
    assert_eq!(item.trace, Some(trace));
}

#[test]
fn response_gets_own_unique_id() {
    let req = Frame::request("test:op");
    let a = req.item(HashMap::new());
    let b = req.item(HashMap::new());
    assert_ne!(a.id, b.id);
    assert_ne!(a.id, req.id);
}

#[test]
fn timestamp_is_positive() {
    let f = Frame::request("test:op");
    assert!(f.ts > 0);
}

#[test]
fn with_field_serializes_primitives() {
    let f = Frame::request("test:op")
        .with_field("name", "alice")
        .unwrap()
        .with_field("count", 42)
        .unwrap()
        .with_field("active", true)
        .unwrap();
    assert_eq!(f.data.get("name").and_then(Value::as_str), Some("alice"));
    assert_eq!(f.data.get("count").and_then(Value::as_i64), Some(42));
    assert_eq!(
        f.data.get("active").and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn item_from_serializes_struct() {
    #[derive(serde::Serialize)]
    struct Entry {
        id: u32,
        label: String,
    }

    let req = Frame::request("test:op");
    let item = req
        .item_from(&Entry {
            id: 1,
            label: "hello".into(),
        })
        .unwrap();
    assert_eq!(item.status, Status::Item);
    assert_eq!(item.parent_id, Some(req.id));
    assert_eq!(item.data.get("id").and_then(Value::as_u64), Some(1));
    assert_eq!(
        item.data.get("label").and_then(Value::as_str),
        Some("hello")
    );
}

#[test]
fn item_from_wraps_non_object() {
    let req = Frame::request("test:op");
    let item = req.item_from(&42).unwrap();
    assert_eq!(item.data.get("value").and_then(Value::as_i64), Some(42));
}

#[test]
fn done_from_serializes_struct() {
    #[derive(serde::Serialize)]
    struct Summary {
        total: u32,
    }

    let req = Frame::request("test:op");
    let done = req.done_from(&Summary { total: 5 }).unwrap();
    assert_eq!(done.status, Status::Done);
    assert!(done.status.is_terminal());
    assert_eq!(done.data.get("total").and_then(Value::as_u64), Some(5));
}

#[test]
fn bulk_from_serializes_struct() {
    #[derive(serde::Serialize)]
    struct Batch {
        count: u32,
    }

    let req = Frame::request("test:op");
    let bulk = req.bulk_from(&Batch { count: 10 }).unwrap();
    assert_eq!(bulk.status, Status::Bulk);
    assert_eq!(bulk.data.get("count").and_then(Value::as_u64), Some(10));
}

#[test]
fn with_field_returns_serialize_error() {
    struct FailingValue;

    impl serde::Serialize for FailingValue {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("boom"))
        }
    }

    let err = Frame::request("test:op")
        .with_field("bad", FailingValue)
        .unwrap_err();
    assert_eq!(err.to_string(), "boom");
}

#[test]
fn item_from_returns_serialize_error() {
    struct FailingValue;

    impl serde::Serialize for FailingValue {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("boom"))
        }
    }

    let req = Frame::request("test:op");
    let err = req.item_from(&FailingValue).unwrap_err();
    assert_eq!(err.to_string(), "boom");
}

#[test]
fn frame_serde_round_trip() {
    let f = Frame::request("test:op")
        .with_from("user-1")
        .with_data("count", Value::Number(42.into()));
    let json = serde_json::to_string(&f).expect("serialize");
    let restored: Frame = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.id, f.id);
    assert_eq!(restored.syscall, f.syscall);
    assert_eq!(restored.status, f.status);
    assert_eq!(restored.from, f.from);
    assert_eq!(restored.data.get("count"), f.data.get("count"));
}
