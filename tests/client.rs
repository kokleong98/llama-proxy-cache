//! Direct tests of `LlamaClient` against the mock llama.cpp backend.

use futures::StreamExt;
use lpcache::llama_client::{BackendError, JsonChat, LlamaBackend, LlamaClient};
use lpcache::mock_backend::MockLlama;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;

async fn client_and_mock() -> (LlamaClient, MockLlama) {
    let mock = MockLlama::start("mock-model").await;
    let c = LlamaClient::new(&mock.url(), Duration::from_secs(10), None).expect("client");
    (c, mock)
}

#[tokio::test]
async fn get_model_id_from_backend() {
    let (c, _m) = client_and_mock().await;
    assert_eq!(c.get_model_id().await, "mock-model");
}

#[tokio::test]
async fn get_model_id_unknown_when_backend_down() {
    let c = LlamaClient::new("http://127.0.0.1:1", Duration::from_secs(2), None).expect("client");
    assert_eq!(c.get_model_id().await, "unknown");
}

/// One-shot raw-TCP server: accepts a single connection, captures the
/// request head (up to the CRLFCRLF) and answers 200 with an empty JSON
/// object. Returns (base_url, handle that yields the captured request).
fn one_shot_capture_server() -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut socket, _) = listener.accept().expect("accept");
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let mut tmp = [0u8; 4096];
            let n = socket.read(&mut tmp).expect("read");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let req = String::from_utf8_lossy(&buf).into_owned();
        let _ = socket.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        );
        req
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn api_key_sends_authorization_header() {
    let (url, handle) = one_shot_capture_server();
    let c = LlamaClient::new(&url, Duration::from_secs(5), Some("777")).expect("client");
    let _ = c.get_model_id().await;
    let req = handle.join().expect("join");
    assert!(
        req.to_ascii_lowercase()
            .contains("authorization: bearer 777"),
        "request did not carry the API key header: {req}"
    );
}

#[tokio::test]
async fn no_api_key_sends_no_authorization_header() {
    let (url, handle) = one_shot_capture_server();
    let c = LlamaClient::new(&url, Duration::from_secs(5), None).expect("client");
    let _ = c.get_model_id().await;
    let req = handle.join().expect("join");
    assert!(
        !req.to_ascii_lowercase().contains("authorization"),
        "request unexpectedly carried an Authorization header: {req}"
    );
}

#[tokio::test]
async fn chat_json_object_passthrough_and_slot_pinning() {
    let (c, mock) = client_and_mock().await;
    let out = c
        .chat_json(&json!({"model": "m", "messages": []}), Some(0))
        .await
        .expect("ok");
    match out {
        JsonChat::Object(v) => {
            assert_eq!(v["object"], "chat.completion");
            assert_eq!(v["model"], "m");
            assert_eq!(v["choices"][0]["message"]["content"], "mock reply");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    // slot pin duplicated in body root, options and query (as in Python)
    let calls = mock.state.calls.lock().await;
    let sent = &calls.chat_bodies[0];
    assert_eq!(sent["_slot_id"], 0);
    assert_eq!(sent["slot_id"], 0);
    assert_eq!(sent["id_slot"], 0);
    assert_eq!(sent["options"]["slot_id"], 0);
    assert_eq!(sent["options"]["id_slot"], 0);
    let q = &calls.chat_queries[0];
    assert!(q.contains(&("slot_id".to_string(), "0".to_string())));
    assert!(q.contains(&("id_slot".to_string(), "0".to_string())));
}

#[tokio::test]
async fn chat_json_no_pinning_when_slot_none() {
    let (c, mock) = client_and_mock().await;
    let out = c.chat_json(&json!({"model": "m"}), None).await.expect("ok");
    assert!(matches!(out, JsonChat::Object(_)));
    let calls = mock.state.calls.lock().await;
    let sent: &Value = &calls.chat_bodies[0];
    assert!(sent.get("_slot_id").is_none());
    assert!(sent.get("slot_id").is_none());
    assert!(sent.get("options").is_none());
    assert!(calls.chat_queries[0].is_empty());
}

#[tokio::test]
async fn chat_json_non_json_content_type() {
    let (c, mock) = client_and_mock().await;
    mock.state
        .set_raw_response("text/plain", "plain body")
        .await;
    let out = c.chat_json(&json!({}), None).await.expect("ok");
    match out {
        JsonChat::NonJson { message, raw } => {
            assert_eq!(message, "provider returned non-JSON");
            assert!(raw.contains("plain body"));
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[tokio::test]
async fn chat_json_malformed_json_body() {
    let (c, mock) = client_and_mock().await;
    mock.state
        .set_raw_response("application/json", "{broken")
        .await;
    let out = c.chat_json(&json!({}), None).await.expect("ok");
    match out {
        JsonChat::NonJson { message, raw } => {
            assert_eq!(message, "invalid json from provider");
            assert!(raw.contains("{broken"));
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[tokio::test]
async fn chat_json_non_object_json() {
    let (c, mock) = client_and_mock().await;
    mock.state
        .set_raw_response("application/json", "[1,2,3]")
        .await;
    let out = c.chat_json(&json!({}), None).await.expect("ok");
    assert!(matches!(out, JsonChat::NonObject));
}

#[tokio::test]
async fn chat_json_http_error() {
    let (c, mock) = client_and_mock().await;
    mock.state.chat_status.store(503, Ordering::SeqCst);
    let err = c
        .chat_json(&json!({}), None)
        .await
        .expect_err("should fail");
    match err {
        BackendError::HttpStatus { status, body } => {
            assert_eq!(status, 503);
            assert!(body.contains("mock chat failure"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
#[tokio::test]
async fn chat_stream_chunks() {
    let (c, _mock) = client_and_mock().await;
    let sc = c
        .chat_stream(&json!({"model": "m", "stream": true}), Some(1))
        .await
        .expect("ok");
    assert_eq!(sc.status, 200);
    let mut sc = sc;
    let mut chunks: Vec<String> = Vec::new();
    while let Some(chunk) = sc.stream.next().await {
        let b = chunk.expect("chunk");
        chunks.push(String::from_utf8_lossy(&b).to_string());
    }
    let all: String = chunks.concat();
    assert!(all.contains("Hello"), "body: {all}");
    assert!(all.contains("world"), "body: {all}");
    assert!(all.contains("[DONE]"), "body: {all}");
}

#[tokio::test]
async fn chat_stream_error_status() {
    let (c, mock) = client_and_mock().await;
    mock.state.chat_status.store(500, Ordering::SeqCst);
    let sc = c
        .chat_stream(&json!({"stream": true}), None)
        .await
        .expect("ok");
    assert_eq!(sc.status, 500);
    let mut sc = sc;
    let mut text = String::new();
    while let Some(chunk) = sc.stream.next().await {
        if let Ok(b) = chunk {
            text.push_str(&String::from_utf8_lossy(&b));
        }
    }
    assert!(text.contains("mock chat failure"));
}

#[tokio::test]
async fn save_slot_statuses() {
    let (c, mock) = client_and_mock().await;
    // 200 -> true
    assert!(c.save_slot(0, "abc").await.expect("200"));
    // 500 -> Ok(false) (no error, same as Python)
    mock.state.save_status.store(500, Ordering::SeqCst);
    assert!(!c.save_slot(1, "abc").await.expect("500 -> false"));
    // 404 -> Err(HttpStatus) (mirrors raise_for_status)
    mock.state.save_status.store(404, Ordering::SeqCst);
    assert!(matches!(
        c.save_slot(2, "abc").await,
        Err(BackendError::HttpStatus { status: 404, .. })
    ));
    let saves = mock.state.calls.lock().await.saves.clone();
    assert_eq!(
        saves,
        vec![
            (0usize, "abc".to_string()),
            (1usize, "abc".to_string()),
            (2usize, "abc".to_string())
        ]
    );
}

#[tokio::test]
async fn restore_slot_statuses() {
    let (c, mock) = client_and_mock().await;
    // 200 -> true
    assert!(c.restore_slot(0, "abc").await);
    // non-200 -> false (warning, no error)
    mock.state.restore_status.store(500, Ordering::SeqCst);
    assert!(!c.restore_slot(1, "abc").await);
    let restores = mock.state.calls.lock().await.restores.clone();
    assert_eq!(
        restores,
        vec![(0usize, "abc".to_string()), (1usize, "abc".to_string())]
    );
}

#[tokio::test]
async fn base_url_trailing_slash_normalized() {
    let mock = MockLlama::start("mock-model").await;
    let url = format!("{}/", mock.url());
    let c = LlamaClient::new(&url, Duration::from_secs(10), None).expect("client");
    assert_eq!(c.get_model_id().await, "mock-model");
}
