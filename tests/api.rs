//! End-to-end tests: the proxy (axum app) in front of a mock llama.cpp
//! backend, mirroring the request flows in `app.py`.

use axum::body::{Body, to_bytes};
use lpcache::app::{AppState, router};
use lpcache::coalesce::SingleFlight;
use lpcache::config::{BackendConf, Config, DEFAULT_STREAM_QUEUE_SIZE};
use lpcache::llama_client::{LlamaBackend, LlamaClient};
use lpcache::mock_backend::MockLlama;
use lpcache::slot_manager::SlotManager;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// The backend model id reported by the mock (used inside cache keys).
const BACKEND_MODEL: &str = "backend-model";

async fn make_state(
    n_slots: usize,
    acquire_timeout: Option<Duration>,
) -> (AppState, MockLlama, tempfile::TempDir) {
    make_state_inner(n_slots, acquire_timeout, false, DEFAULT_STREAM_QUEUE_SIZE).await
}

/// Like [`make_state`], but with concurrent-request coalescing enabled.
async fn make_state_coalesce(n_slots: usize) -> (AppState, MockLlama, tempfile::TempDir) {
    make_state_inner(n_slots, None, true, DEFAULT_STREAM_QUEUE_SIZE).await
}

/// Like [`make_state`], but with a custom `STREAM_QUEUE_SIZE`.
async fn make_state_stream_queue(
    n_slots: usize,
    stream_queue_size: usize,
) -> (AppState, MockLlama, tempfile::TempDir) {
    make_state_inner(n_slots, None, false, stream_queue_size).await
}

/// Like [`make_state_coalesce`], but with a custom `STREAM_QUEUE_SIZE`.
async fn make_state_coalesce_stream_queue(
    n_slots: usize,
    stream_queue_size: usize,
) -> (AppState, MockLlama, tempfile::TempDir) {
    make_state_inner(n_slots, None, true, stream_queue_size).await
}

async fn make_state_inner(
    n_slots: usize,
    acquire_timeout: Option<Duration>,
    coalesce: bool,
    stream_queue_size: usize,
) -> (AppState, MockLlama, tempfile::TempDir) {
    let mock = MockLlama::start(BACKEND_MODEL).await;
    let td = tempfile::tempdir().expect("tempdir");
    let cfg = Config::new(
        vec![BackendConf {
            url: mock.url(),
            n_slots,
            slot_save_path: None,
        }],
        100, // words per block
        500, // big threshold words
        0.6, // LCP threshold
        td.path().to_path_buf(),
        Duration::from_secs(30),
        "llama.cpp".to_string(),
        0,
    )
    .with_coalesce_requests(coalesce)
    .with_stream_queue_size(stream_queue_size);
    let client = Arc::new(
        LlamaClient::new(&cfg.backends[0].url, cfg.request_timeout, None).expect("client"),
    );
    let clients: Vec<Arc<dyn LlamaBackend>> = vec![Arc::clone(&client) as Arc<dyn LlamaBackend>];
    let sm = SlotManager::new(&cfg.backends, clients.clone());
    let sm = match acquire_timeout {
        Some(d) => sm.with_acquire_timeout(d),
        None => sm,
    };
    let sm = Arc::new(sm);
    let state = AppState {
        config: Arc::new(cfg),
        clients,
        sm,
        sf: Arc::new(SingleFlight::new()),
    };
    (state, mock, td)
}

async fn make_app() -> (axum::Router, MockLlama, tempfile::TempDir) {
    let (state, mock, td) = make_state(2, None).await;
    (router(state), mock, td)
}

/// The proxy served on a real local TCP port. Client-disconnect behaviour
/// requires a real connection: in-process `tower` calls never "disconnect".
struct ServedProxy {
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl ServedProxy {
    async fn serve(app: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, task }
    }
}

impl Drop for ServedProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn post_json(body: &Value) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

/// Grab a free local TCP port (bind :0, read the port, drop the socket).
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
    listener.local_addr().expect("local addr").port()
}

fn get_req(uri: &str) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

/// `n` distinct words: `word0 word1 ... word{n-1}`.
fn words(n: usize) -> String {
    (0..n)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cache key the proxy computes for a single-message conversation.
fn expected_key(prefix: &str) -> String {
    let full = format!("{BACKEND_MODEL}\n{prefix}");
    lpcache::hashing::prefix_key_sha256(&full)
}

async fn read_body(resp: axum::http::Response<Body>) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    serde_json::from_slice(&bytes).expect("json body")
}

#[tokio::test]
async fn models_endpoint_returns_configured_id() {
    let (app, _mock, _td) = make_app().await;
    let resp = app.oneshot(get_req("/v1/models")).await.expect("resp");
    assert_eq!(resp.status(), 200);
    let v = read_body(resp).await;
    assert_eq!(v, json!({ "data": [{ "id": "llama.cpp" }] }));
}

#[tokio::test]
async fn nonstream_small_request_no_save_no_meta() {
    let (app, mock, td) = make_app().await;
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "hello world" }]
    });
    let resp = app.oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status(), 200);
    let v = read_body(resp).await;
    assert_eq!(v["choices"][0]["message"]["content"], "mock reply");

    let calls = mock.state.calls.lock().await;
    assert_eq!(calls.chat_bodies.len(), 1);
    let sent = &calls.chat_bodies[0];
    // root fields (as set by app.py + the client's slot pin)
    assert_eq!(sent["model"], "m1");
    assert_eq!(sent["cache_prompt"], false);
    assert_eq!(sent["n_keep"], -1);
    assert_eq!(sent["_slot_id"], 0);
    assert_eq!(sent["slot_id"], 0);
    assert_eq!(sent["id_slot"], 0);
    // options
    assert_eq!(sent["options"]["slot_id"], 0);
    assert_eq!(sent["options"]["id_slot"], 0);
    assert_eq!(sent["options"]["n_keep"], -1);
    assert_eq!(sent["options"]["cache_prompt"], false);
    // query
    let q = &calls.chat_queries[0];
    assert!(q.contains(&("slot_id".to_string(), "0".to_string())));
    assert!(q.contains(&("id_slot".to_string(), "0".to_string())));
    // small request: no save, no restore, no meta files
    assert!(calls.saves.is_empty());
    assert!(calls.restores.is_empty());
    assert_eq!(std::fs::read_dir(td.path()).expect("read dir").count(), 0);
}

#[tokio::test]
async fn nonstream_big_request_saves_and_writes_meta() {
    let (app, mock, td) = make_app().await;
    let content = words(600); // > 500 -> "big"
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": content.clone() }]
    });
    let key = expected_key(&content);

    let resp = app.oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status(), 200);
    let v = read_body(resp).await;
    assert_eq!(v["choices"][0]["message"]["content"], "mock reply");

    let calls = mock.state.calls.lock().await;
    assert_eq!(calls.saves, vec![(0usize, key.clone())]);
    assert_eq!(calls.chat_bodies[0]["cache_prompt"], true);
    assert!(calls.restores.is_empty());

    let meta_path = td.path().join(format!("{key}.meta.json"));
    assert!(meta_path.exists(), "meta file missing: {meta_path:?}");
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).expect("read meta"))
            .expect("meta json");
    assert_eq!(meta["key"], key);
    assert_eq!(meta["model_id"], BACKEND_MODEL);
    assert_eq!(meta["wpb"], 100);
    assert_eq!(meta["blocks"].as_array().expect("blocks").len(), 6);
    assert_eq!(meta["prefix_len"], content.chars().count());
    assert!(meta["timestamp"].as_f64().expect("ts") > 0.0);
}
#[tokio::test]
async fn second_big_request_restores_saved_cache() {
    let (app, mock, _td) = make_app().await;
    let a = words(600);
    // 1st request: no meta exists yet -> no restore, saves key_a
    let body1 = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": a.clone() }]
    });
    let resp = app.clone().oneshot(post_json(&body1)).await.expect("resp");
    assert_eq!(resp.status(), 200);
    let _ = to_bytes(resp.into_body(), usize::MAX).await.expect("body");

    let key_a = expected_key(&a);
    assert_eq!(
        mock.state.calls.lock().await.saves.clone(),
        vec![(0usize, key_a.clone())]
    );

    // 2nd request: same prefix + extra tail -> LCP ratio 1.0 >= 0.6
    // -> restore key_a into the next free slot (0,1)
    let b = format!("{a} tail x y z");
    let body2 = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": b.clone() }]
    });
    let resp = app.clone().oneshot(post_json(&body2)).await.expect("resp");
    assert_eq!(resp.status(), 200);
    let _ = to_bytes(resp.into_body(), usize::MAX).await.expect("body");

    let calls = mock.state.calls.lock().await;
    // request 1 had no candidate, so this restore belongs to request 2
    assert_eq!(calls.restores, vec![(1usize, key_a)]);
    assert_eq!(calls.chat_bodies.len(), 2);
    assert_eq!(calls.chat_bodies[1]["options"]["slot_id"], 1);
    assert_eq!(calls.chat_bodies[1]["cache_prompt"], true);
    // and the new key was saved after the 2nd chat
    let key_b = expected_key(&b);
    assert_eq!(
        calls.saves,
        vec![(0usize, expected_key(&a)), (1usize, key_b)]
    );
}

#[tokio::test]
async fn stream_request_passes_sse_and_saves_afterwards() {
    let (app, mock, td) = make_app().await;
    let body = json!({
        "model": "m1",
        "stream": true,
        "messages": [{ "role": "user", "content": "hi there" }]
    });
    let resp = app.oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status(), 200);
    let ctype = resp
        .headers()
        .get("content-type")
        .expect("content-type")
        .to_str()
        .expect("str")
        .to_string();
    assert!(
        ctype.starts_with("text/event-stream"),
        "content-type: {ctype}"
    );

    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("stream body");
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("Hello"), "body: {text}");
    assert!(text.contains("world"), "body: {text}");
    assert!(text.contains("[DONE]"), "body: {text}");

    // The reader task saves + writes meta after the stream ends — always,
    // even for small requests (same as the Python reader task).
    let key = expected_key("hi there");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !mock.state.saves().await.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for save"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(mock.state.saves().await, vec![(0usize, key.clone())]);
    assert!(td.path().join(format!("{key}.meta.json")).exists());
}

#[tokio::test]
async fn nonstream_client_cancel_aborts_upstream() {
    // Single slot + short acquire timeout: if the cancelled handler leaked
    // its slot, the follow-up request would 503 instead of 200.
    let (state, mock, _td) = make_state(1, Some(Duration::from_secs(2))).await;
    let proxy = ServedProxy::serve(router(state)).await;

    // The mock takes 1 s to produce the JSON body: the proxy's request to
    // the backend stays in flight for that whole second.
    mock.state.set_chat_body_delay(Duration::from_millis(1000));
    mock.state.reset_json_bodies_produced();

    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/chat/completions", proxy.addr);
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "hi there" }]
    });

    // Start the request, let it reach the backend, then cancel it (dropping
    // the in-flight reqwest future = the client going away).
    let send = client.post(&url).json(&body).send();
    let handle = tokio::spawn(send);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        mock.state.chat_bodies().await.len(),
        1,
        "request must reach the backend before the cancel"
    );
    handle.abort();

    // The proxy must abort its request to the backend: the mock must never
    // produce the JSON body.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        mock.state.json_bodies_produced(),
        0,
        "upstream request must be aborted when the client cancels"
    );

    // The slot must have been released by the cancelled handler (its guard
    // is dropped with the future), so the follow-up succeeds on the single
    // slot instead of timing out (-> 503).
    mock.state.set_chat_body_delay(Duration::ZERO);
    let follow = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("follow-up request");
    assert_eq!(follow.status(), 200);
    let _ = follow.bytes().await.expect("follow-up body");
}

#[tokio::test]
async fn stream_client_cancel_aborts_upstream_over_tcp() {
    use futures::StreamExt;

    let (app, mock, td) = make_app().await;
    let proxy = ServedProxy::serve(app).await;

    // 3 chunks, 500 ms apart: without cancellation the mock would produce
    // all of them even after the client is gone.
    mock.state
        .set_stream_chunk_delay(Duration::from_millis(500));
    mock.state.reset_chunks_produced();

    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/chat/completions", proxy.addr);
    let body = json!({
        "model": "m1",
        "stream": true,
        "messages": [{ "role": "user", "content": "hi there" }]
    });

    // Client: open the stream, read the first chunk, then disconnect (drop
    // the response = close the connection).
    let send = client.post(&url).json(&body).send();
    let handle = tokio::spawn(async move {
        let resp = send.await.expect("response");
        let mut data = resp.bytes_stream();
        let first = data.next().await.expect("first chunk").expect("bytes");
        assert!(!first.is_empty());
        // dropping the response closes the connection
    });
    // Wait for the first chunk (~500 ms) and the task to drop the response.
    tokio::time::sleep(Duration::from_millis(800)).await;
    handle.await.expect("client task");

    // The proxy must abort the upstream stream right away: the mock never
    // produces chunk 2 (which would appear ~500 ms after chunk 1).
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        mock.state.chunks_produced(),
        1,
        "upstream stream must be aborted when the client disconnects"
    );
    // ...and it stays aborted (not merely paused).
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(mock.state.chunks_produced(), 1);

    // The reader's cleanup still ran: KV save + meta, same as a normal
    // stream end.
    let key = expected_key("hi there");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !mock.state.saves().await.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for save after client disconnect"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(mock.state.saves().await, vec![(0usize, key.clone())]);
    assert!(td.path().join(format!("{key}.meta.json")).exists());
}

#[tokio::test]
async fn stream_provider_error_is_passed_through() {
    let (app, mock, _td) = make_app().await;
    mock.state
        .chat_status
        .store(500, std::sync::atomic::Ordering::SeqCst);
    let body = json!({
        "model": "m1",
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let resp = app.oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status(), 500);
    let v = read_body(resp).await;
    assert!(
        v["error"]
            .as_str()
            .expect("error")
            .contains("mock chat failure")
    );
    assert!(mock.state.saves().await.is_empty());
}

#[tokio::test]
async fn nonstream_provider_error_becomes_500() {
    let (app, mock, _td) = make_app().await;
    mock.state
        .chat_status
        .store(500, std::sync::atomic::Ordering::SeqCst);
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let resp = app.oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status(), 500);
    let v = read_body(resp).await;
    assert!(v["error"].as_str().expect("error").contains("500"));
    assert!(mock.state.saves().await.is_empty());
}

#[tokio::test]
async fn nonstream_json_array_provider_returns_502() {
    let (app, mock, _td) = make_app().await;
    mock.state
        .set_raw_response("application/json", "[1,2,3]")
        .await;
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let resp = app.oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status(), 502);
    let v = read_body(resp).await;
    assert_eq!(v["error"], "provider non-JSON body");
    assert!(mock.state.saves().await.is_empty());
}

#[tokio::test]
async fn invalid_json_body_rejected() {
    let (app, _mock, _td) = make_app().await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from("not json"))
        .expect("request");
    let resp = app.oneshot(req).await.expect("resp");
    assert_eq!(resp.status(), 422);
}

#[tokio::test]
async fn model_defaults_to_configured_id_when_missing() {
    let (app, mock, _td) = make_app().await;
    let body = json!({
        "stream": false,
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let resp = app.oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status(), 200);
    let v = read_body(resp).await;
    assert_eq!(v["choices"][0]["message"]["content"], "mock reply");
    // and the configured MODEL_ID is forwarded to the backend
    let sent = &mock.state.chat_bodies().await[0];
    assert_eq!(sent["model"], "llama.cpp");
}

#[tokio::test]
async fn dead_backend_traffic_fails_over_to_live_backend() {
    // backend 0 = live mock, backend 1 = closed port (connection refused).
    // Big requests so saves stamp `last_used` and the free/LRU paths rotate.
    let mock = MockLlama::start(BACKEND_MODEL).await;
    let td = tempfile::tempdir().expect("tempdir");
    let cfg = Config::new(
        vec![
            BackendConf {
                url: mock.url(),
                n_slots: 1,
                slot_save_path: None,
            },
            BackendConf {
                url: "http://127.0.0.1:9".to_string(),
                n_slots: 1,
                slot_save_path: None,
            },
        ],
        100,
        500,
        0.6,
        td.path().to_path_buf(),
        Duration::from_secs(30),
        "llama.cpp".to_string(),
        0,
    );
    let c0 =
        Arc::new(LlamaClient::new(&cfg.backends[0].url, cfg.request_timeout, None).expect("c0"));
    let c1 =
        Arc::new(LlamaClient::new(&cfg.backends[1].url, cfg.request_timeout, None).expect("c1"));
    let clients: Vec<Arc<dyn LlamaBackend>> = vec![
        Arc::clone(&c0) as Arc<dyn LlamaBackend>,
        Arc::clone(&c1) as Arc<dyn LlamaBackend>,
    ];
    let sm = Arc::new(
        SlotManager::new(&cfg.backends, clients.clone())
            .with_backend_cooldown(Duration::from_secs(60)),
    );
    let state = AppState {
        config: Arc::new(cfg),
        clients,
        sm,
        sf: Arc::new(SingleFlight::new()),
    };
    let app = router(state);

    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": words(600) }]
    });
    let mut statuses = Vec::new();
    for _ in 0..4 {
        let resp = app.clone().oneshot(post_json(&body)).await.expect("resp");
        statuses.push(resp.status().as_u16());
        let _ = to_bytes(resp.into_body(), usize::MAX).await;
    }
    // req1 -> (0,0) live -> 200 (save stamps slot (0,0) used)
    // req2 -> (1,0) free -> chat: connection refused -> breaker(be1) ->
    //        retried on (0,0) live -> 200 (transparent failover; the retry
    //        re-restores the key there)
    // req3/4 -> be1 in cooldown -> (0,0) -> 200
    assert_eq!(statuses, vec![200, 200, 200, 200]);
    // all chats (including the retry) landed on the live backend
    let saves = mock.state.saves().await;
    assert_eq!(saves.len(), 4);
    // req2's retry + req3 + req4 restored on the live backend
    let restores = mock.state.restores().await;
    assert_eq!(restores.len(), 3);
}

#[tokio::test]
async fn connection_failure_retries_on_other_backend() {
    // backend 0 = dead (its port is released -> connection refused),
    // backend 1 = live mock. A connection-level failure must be retried
    // once on the live backend, so the client sees 200 instead of 500.
    let port = free_port();
    let dead_url = format!("http://127.0.0.1:{port}");
    let live = MockLlama::start(BACKEND_MODEL).await;
    let td = tempfile::tempdir().expect("tempdir");
    let cfg = Config::new(
        vec![
            BackendConf {
                url: dead_url,
                n_slots: 1,
                slot_save_path: None,
            },
            BackendConf {
                url: live.url(),
                n_slots: 1,
                slot_save_path: None,
            },
        ],
        100,
        500,
        0.6,
        td.path().to_path_buf(),
        Duration::from_secs(30),
        "llama.cpp".to_string(),
        0,
    );
    let c0 =
        Arc::new(LlamaClient::new(&cfg.backends[0].url, cfg.request_timeout, None).expect("c0"));
    let c1 =
        Arc::new(LlamaClient::new(&cfg.backends[1].url, cfg.request_timeout, None).expect("c1"));
    let clients: Vec<Arc<dyn LlamaBackend>> = vec![
        Arc::clone(&c0) as Arc<dyn LlamaBackend>,
        Arc::clone(&c1) as Arc<dyn LlamaBackend>,
    ];
    let sm = Arc::new(SlotManager::new(&cfg.backends, clients.clone()));
    let state = AppState {
        config: Arc::new(cfg),
        clients,
        sm,
        sf: Arc::new(SingleFlight::new()),
    };
    let app = router(state);

    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "hello" }]
    });
    let resp = app.clone().oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(
        resp.status(),
        200,
        "the retry on the live backend must succeed"
    );
    let _ = to_bytes(resp.into_body(), usize::MAX).await;
    // exactly one chat reached a backend (the retry on the live one)
    assert_eq!(live.state.chat_bodies().await.len(), 1);
}

#[tokio::test]
async fn probe_recovers_cooled_down_backend() {
    // backend 0 = closed port, backend 1 = live mock. A failed request
    // trips backend 0's breaker (and transparently retries on backend 1);
    // probes report it as still down; once a server appears on its port,
    // one probe round recovers it and traffic returns to it.
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
    let dead_url = format!("http://{addr}");
    let live = MockLlama::start(BACKEND_MODEL).await;
    let td = tempfile::tempdir().expect("tempdir");
    let cfg = Config::new(
        vec![
            BackendConf {
                url: dead_url,
                n_slots: 1,
                slot_save_path: None,
            },
            BackendConf {
                url: live.url(),
                n_slots: 1,
                slot_save_path: None,
            },
        ],
        100,
        500,
        0.6,
        td.path().to_path_buf(),
        Duration::from_secs(30),
        "llama.cpp".to_string(),
        0,
    );
    let c0 =
        Arc::new(LlamaClient::new(&cfg.backends[0].url, cfg.request_timeout, None).expect("c0"));
    let c1 =
        Arc::new(LlamaClient::new(&cfg.backends[1].url, cfg.request_timeout, None).expect("c1"));
    let clients: Vec<Arc<dyn LlamaBackend>> = vec![
        Arc::clone(&c0) as Arc<dyn LlamaBackend>,
        Arc::clone(&c1) as Arc<dyn LlamaBackend>,
    ];
    let sm = Arc::new(
        SlotManager::new(&cfg.backends, clients.clone())
            .with_backend_cooldown(Duration::from_secs(60)),
    );
    let state = AppState {
        config: Arc::new(cfg),
        clients,
        sm: Arc::clone(&sm),
        sf: Arc::new(SingleFlight::new()),
    };
    let app = router(state);

    // same big prompt for all requests (one cache key; saves stamp slots)
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": words(600) }]
    });
    // req1: (0,0) dead -> breaker + transparent retry on (1,0) -> 200
    for i in 0..3 {
        let resp = app.clone().oneshot(post_json(&body)).await.expect("resp");
        assert_eq!(resp.status(), 200, "request {i}");
        let _ = to_bytes(resp.into_body(), usize::MAX).await;
    }
    // all saves landed on the live backend while be0 cooled down
    assert_eq!(live.state.saves().await.len(), 3);

    // probe round 1: backend 0 is still down
    assert_eq!(sm.probe_down_backends().await, 0);

    // backend 0 comes back on the same port
    let recovered = MockLlama::start_on(BACKEND_MODEL, addr).await;
    assert_eq!(sm.probe_down_backends().await, 1, "probe must recover it");

    // req4: both slots are stamped now, so LRU picks the least recently
    // used one — (0,0), i.e. the just-recovered backend 0
    let resp = app.clone().oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status(), 200);
    let _ = to_bytes(resp.into_body(), usize::MAX).await;
    assert_eq!(recovered.state.chat_bodies().await.len(), 1);
}

#[tokio::test]
async fn failing_backend_slots_do_not_pin_all_traffic() {
    // backend 0 = healthy mock, backend 1 = mock whose chats return HTTP 500.
    // A provider error does not trip the circuit breaker (the backend is
    // reachable), but the failed slot must be marked used so selection moves
    // on instead of pinning every request to the failing slot.
    let mock0 = MockLlama::start(BACKEND_MODEL).await;
    let mock1 = MockLlama::start(BACKEND_MODEL).await;
    mock1
        .state
        .chat_status
        .store(500, std::sync::atomic::Ordering::SeqCst);
    let td = tempfile::tempdir().expect("tempdir");
    let cfg = Config::new(
        vec![
            BackendConf {
                url: mock0.url(),
                n_slots: 2,
                slot_save_path: None,
            },
            BackendConf {
                url: mock1.url(),
                n_slots: 2,
                slot_save_path: None,
            },
        ],
        100,
        500,
        0.6,
        td.path().to_path_buf(),
        Duration::from_secs(30),
        "llama.cpp".to_string(),
        0,
    );
    let c0 =
        Arc::new(LlamaClient::new(&cfg.backends[0].url, cfg.request_timeout, None).expect("c0"));
    let c1 =
        Arc::new(LlamaClient::new(&cfg.backends[1].url, cfg.request_timeout, None).expect("c1"));
    let clients: Vec<Arc<dyn LlamaBackend>> = vec![
        Arc::clone(&c0) as Arc<dyn LlamaBackend>,
        Arc::clone(&c1) as Arc<dyn LlamaBackend>,
    ];
    let sm = Arc::new(SlotManager::new(&cfg.backends, clients.clone()));
    let state = AppState {
        config: Arc::new(cfg),
        clients,
        sm,
        sf: Arc::new(SingleFlight::new()),
    };
    let app = router(state);

    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": words(600) }]
    });
    let mut statuses = Vec::new();
    for _ in 0..5 {
        let resp = app.clone().oneshot(post_json(&body)).await.expect("resp");
        statuses.push(resp.status().as_u16());
        let _ = to_bytes(resp.into_body(), usize::MAX).await;
    }
    // req1 -> (0,0) 200 (save stamps it); req2 -> (0,1) 200 (save stamps it)
    // req3 -> (1,0) free -> provider 500 -> slot marked used
    // req4 -> (1,1) free -> provider 500 -> slot marked used
    // req5 -> no free slots -> LRU -> (0,0) -> 200
    // (without mark_used, req5 would have re-picked the still-"free" (1,0))
    assert_eq!(statuses, vec![200, 200, 500, 500, 200]);
    assert_eq!(mock0.state.saves().await.len(), 3);
    assert_eq!(mock1.state.saves().await.len(), 0);
}

#[tokio::test]
async fn prune_removes_oldest_meta_and_kv_files() {
    // mock emulates --slot-save-path: each successful save creates a KV file
    let kv = tempfile::tempdir().expect("kv dir");
    let mock = MockLlama::start_with_save_dir(BACKEND_MODEL, kv.path()).await;
    let td = tempfile::tempdir().expect("meta dir");
    let cfg = Config::new(
        vec![BackendConf {
            url: mock.url(),
            n_slots: 2,
            slot_save_path: Some(kv.path().to_string_lossy().into_owned()),
        }],
        100, // words per block
        500, // big threshold words
        0.6, // LCP threshold
        td.path().to_path_buf(),
        Duration::from_secs(30),
        "llama.cpp".to_string(),
        0,
    )
    .with_meta_max(2);
    let client = Arc::new(
        LlamaClient::new(&cfg.backends[0].url, cfg.request_timeout, None).expect("client"),
    );
    let clients: Vec<Arc<dyn LlamaBackend>> = vec![Arc::clone(&client) as Arc<dyn LlamaBackend>];
    let sm = Arc::new(SlotManager::new(&cfg.backends, clients.clone()));
    let state = AppState {
        config: Arc::new(cfg),
        clients,
        sm,
        sf: Arc::new(SingleFlight::new()),
    };
    let app = router(state);

    // three distinct "big" requests (each > 500 words, first block differs
    // so none of them restores a previous one)
    let mut keys = Vec::new();
    for i in 0..3 {
        let prefix = format!("prompt{i} {}", words(518));
        keys.push(expected_key(&prefix));
        let body = json!({
            "model": "m1",
            "stream": false,
            "messages": [{ "role": "user", "content": prefix }]
        });
        let resp = app.clone().oneshot(post_json(&body)).await.expect("resp");
        assert_eq!(resp.status(), 200);
        // make the LRU order deterministic (write order)
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(mock.state.saves().await.len(), 3);

    // META_MAX=2: the oldest entry (request 0) is pruned, meta AND KV file
    assert!(
        !td.path().join(format!("{}.meta.json", keys[0])).exists(),
        "oldest meta file should be pruned"
    );
    assert!(
        !kv.path().join(&keys[0]).exists(),
        "oldest KV file should be pruned"
    );
    // the two newer entries survive in both places
    for key in &keys[1..] {
        assert!(td.path().join(format!("{key}.meta.json")).exists());
        assert!(kv.path().join(key).exists());
    }
}

// ---------------------------------------------------------------------------
// Concurrent-request coalescing (COALESCE_REQUESTS)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn coalesce_concurrent_identical_requests_single_backend_call() {
    let (state, mock, _td) = make_state_coalesce(2).await;
    let app = router(state);
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "coalesce please" }]
    });
    // slow the backend so all 5 requests overlap in time
    mock.state.set_chat_body_delay(Duration::from_millis(400));
    let mut futs = Vec::new();
    for _ in 0..5 {
        let app = app.clone();
        let body = body.clone();
        futs.push(tokio::spawn(async move {
            app.oneshot(post_json(&body)).await.expect("resp")
        }));
    }
    let mut bodies = Vec::new();
    for f in futs {
        let r = f.await.expect("join");
        assert_eq!(r.status().as_u16(), 200);
        bodies.push(to_bytes(r.into_body(), usize::MAX).await.expect("body"));
    }
    // the whole group is ONE backend call
    assert_eq!(mock.state.calls.lock().await.chat_bodies.len(), 1);
    // every follower got the exact result the leader got
    for b in &bodies[1..] {
        assert_eq!(&bodies[0], b);
    }
    let v: Value = serde_json::from_slice(&bodies[0]).expect("json");
    assert_eq!(v["choices"][0]["message"]["content"], "mock reply");
    assert_eq!(v["model"], "m1");
}

#[tokio::test]
async fn coalesce_different_generation_params_still_merge() {
    let (state, mock, _td) = make_state_coalesce(2).await;
    let app = router(state);
    let body1 = json!({
        "model": "m1",
        "stream": false,
        "temperature": 0.1,
        "messages": [{ "role": "user", "content": "coalesce please" }]
    });
    let body2 = json!({
        "model": "m1",
        "stream": false,
        "temperature": 0.9,
        "max_tokens": 7,
        "messages": [{ "role": "user", "content": "coalesce please" }]
    });
    mock.state.set_chat_body_delay(Duration::from_millis(400));
    let (r1, r2) = tokio::join!(
        app.clone().oneshot(post_json(&body1)),
        app.clone().oneshot(post_json(&body2))
    );
    let r1 = r1.expect("resp");
    let r2 = r2.expect("resp");
    assert_eq!(r1.status().as_u16(), 200);
    assert_eq!(r2.status().as_u16(), 200);
    // grouping is cache-key based: different params do NOT break the group
    assert_eq!(mock.state.calls.lock().await.chat_bodies.len(), 1);
    // both callers get the leader's result
    let v1: Value =
        serde_json::from_slice(&to_bytes(r1.into_body(), usize::MAX).await.expect("b")).expect("j");
    let v2: Value =
        serde_json::from_slice(&to_bytes(r2.into_body(), usize::MAX).await.expect("b")).expect("j");
    assert_eq!(v1["choices"][0]["message"]["content"], "mock reply");
    assert_eq!(v2["choices"][0]["message"]["content"], "mock reply");
    assert_eq!(v1["model"], "m1");
    assert_eq!(v2["model"], "m1");
}

#[tokio::test]
async fn coalesce_mixed_stream_and_json_share_one_backend_call() {
    let (state, mock, _td) = make_state_coalesce(2).await;
    let app = router(state);
    let stream_body = json!({
        "model": "m1",
        "stream": true,
        "messages": [{ "role": "user", "content": "mixed group" }]
    });
    let json_body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "mixed group" }]
    });
    // stretch the mock stream so both requests overlap in time
    mock.state.set_stream_chunk_delay(Duration::from_millis(30));
    mock.state.set_chat_body_delay(Duration::from_millis(200));
    let (r_stream, r_json) = tokio::join!(
        app.clone().oneshot(post_json(&stream_body)),
        app.clone().oneshot(post_json(&json_body))
    );
    let r_stream = r_stream.expect("resp");
    let r_json = r_json.expect("resp");
    assert_eq!(r_stream.status().as_u16(), 200);
    assert_eq!(r_json.status().as_u16(), 200);
    // one backend call for the whole (mixed) group
    assert_eq!(mock.state.calls.lock().await.chat_bodies.len(), 1);
    // the JSON caller gets a completion with the shared content
    let v: Value =
        serde_json::from_slice(&to_bytes(r_json.into_body(), usize::MAX).await.expect("b"))
            .expect("j");
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .expect("content");
    assert!(!content.is_empty(), "empty shared content");
    assert_eq!(v["model"], "m1");
    // the stream caller gets a full SSE ending with [DONE]
    let s = String::from_utf8(
        to_bytes(r_stream.into_body(), usize::MAX)
            .await
            .expect("b")
            .to_vec(),
    )
    .expect("utf8");
    assert!(s.contains("data:"), "stream: {s}");
    assert!(s.ends_with("data: [DONE]\n\n"), "stream: {s}");
}

#[tokio::test]
async fn coalesce_stream_groups_share_one_backend_stream() {
    let (state, mock, _td) = make_state_coalesce(2).await;
    let app = router(state);
    let body = json!({
        "model": "m1",
        "stream": true,
        "messages": [{ "role": "user", "content": "stream coalesce" }]
    });
    // stretch the mock stream so all requests overlap in time
    mock.state.set_stream_chunk_delay(Duration::from_millis(30));
    let mut futs = Vec::new();
    for _ in 0..3 {
        let app = app.clone();
        let body = body.clone();
        futs.push(tokio::spawn(async move {
            app.oneshot(post_json(&body)).await.expect("resp")
        }));
    }
    let mut streams = Vec::new();
    for f in futs {
        let r = f.await.expect("join");
        assert_eq!(r.status().as_u16(), 200);
        streams.push(to_bytes(r.into_body(), usize::MAX).await.expect("body"));
    }
    // one backend stream for the whole group
    assert_eq!(mock.state.calls.lock().await.chat_bodies.len(), 1);
    // every follower received the same full stream, ending with [DONE]
    for s in &streams[1..] {
        assert_eq!(&streams[0], s);
    }
    let s = String::from_utf8(streams[0].to_vec()).expect("utf8");
    assert!(s.contains("Hello"), "stream: {s}");
    assert!(s.ends_with("data: [DONE]\n\n"), "stream: {s}");
}

#[tokio::test]
async fn coalesce_flag_off_preserves_per_request_backend_calls() {
    let (state, mock, _td) = make_state(2, None).await;
    let app = router(state);
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "no coalesce" }]
    });
    mock.state.set_chat_body_delay(Duration::from_millis(300));
    let mut futs = Vec::new();
    for _ in 0..5 {
        let app = app.clone();
        let body = body.clone();
        futs.push(tokio::spawn(async move {
            app.oneshot(post_json(&body)).await.expect("resp")
        }));
    }
    for f in futs {
        assert_eq!(f.await.expect("join").status().as_u16(), 200);
    }
    // coalescing disabled -> one backend call per request
    assert_eq!(mock.state.calls.lock().await.chat_bodies.len(), 5);
}

#[tokio::test]
async fn coalesce_new_group_after_completion() {
    let (state, mock, _td) = make_state_coalesce(2).await;
    let app = router(state);
    let body = json!({
        "model": "m1",
        "stream": false,
        "messages": [{ "role": "user", "content": "sequential" }]
    });
    // first request: leader of group 1
    let r1 = app.clone().oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(r1.status().as_u16(), 200);
    // second request (after group 1 finished): a new group, a new call
    let r2 = app.clone().oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(r2.status().as_u16(), 200);
    assert_eq!(mock.state.calls.lock().await.chat_bodies.len(), 2);
}

#[tokio::test]
async fn stream_queue_size_one_delivers_full_stream() {
    // STREAM_QUEUE_SIZE=1: the per-request channel the background reader
    // pumps SSE bytes through holds a single item, so every send after the
    // first waits for the consumer (backpressure). The stream must still be
    // delivered fully, byte-for-byte, and the reader cleanup (save + meta)
    // must still run.
    let (state, mock, td) = make_state_stream_queue(2, 1).await;
    let app = router(state);
    // stretch the mock stream so the reader keeps the (capacity-1) channel
    // full while the consumer reads
    mock.state.set_stream_chunk_delay(Duration::from_millis(30));
    mock.state.reset_chunks_produced();
    let body = json!({
        "model": "m1",
        "stream": true,
        "messages": [{ "role": "user", "content": "queue size one" }]
    });
    let resp = app.oneshot(post_json(&body)).await.expect("resp");
    assert_eq!(resp.status().as_u16(), 200);
    let text = String::from_utf8(
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("stream body")
            .to_vec(),
    )
    .expect("utf8");
    assert!(text.contains("Hello"), "stream: {text}");
    assert!(text.contains("world"), "stream: {text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "stream: {text}");
    // all three mock chunks were generated exactly once
    assert_eq!(mock.state.chunks_produced(), 3);
    // the reader task ran its cleanup despite the backpressure
    let key = expected_key("queue size one");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !mock.state.saves().await.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for save"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(mock.state.saves().await, vec![(0usize, key.clone())]);
    assert!(td.path().join(format!("{key}.meta.json")).exists());
}

#[tokio::test]
async fn coalesce_stream_queue_size_one_follower_receives_full_stream() {
    // The stream-follower path also bridges the leader's bytes through a
    // channel of STREAM_QUEUE_SIZE capacity. With 1 and a stretched leader
    // stream, every follower send is backpressured, yet each follower must
    // still receive the identical full SSE stream and the backend must be
    // called once for the whole group.
    let (state, mock, _td) = make_state_coalesce_stream_queue(2, 1).await;
    let app = router(state);
    let body = json!({
        "model": "m1",
        "stream": true,
        "messages": [{ "role": "user", "content": "stream queue coalesce" }]
    });
    // stretch the mock stream so all requests overlap in time
    mock.state.set_stream_chunk_delay(Duration::from_millis(30));
    let mut futs = Vec::new();
    for _ in 0..3 {
        let app = app.clone();
        let body = body.clone();
        futs.push(tokio::spawn(async move {
            app.oneshot(post_json(&body)).await.expect("resp")
        }));
    }
    let mut streams = Vec::new();
    for f in futs {
        let r = f.await.expect("join");
        assert_eq!(r.status().as_u16(), 200);
        streams.push(to_bytes(r.into_body(), usize::MAX).await.expect("body"));
    }
    // one backend stream for the whole group
    assert_eq!(mock.state.calls.lock().await.chat_bodies.len(), 1);
    // every follower received the same full stream, ending with [DONE]
    for s in &streams[1..] {
        assert_eq!(&streams[0], s);
    }
    let s = String::from_utf8(streams[0].to_vec()).expect("utf8");
    assert!(s.contains("Hello"), "stream: {s}");
    assert!(s.ends_with("data: [DONE]\n\n"), "stream: {s}");
}
