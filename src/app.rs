//! HTTP API (port of `app.py`).
//!
//! - `GET /v1/models` -> `{"data": [{"id": MODEL_ID}]}`
//! - `POST /v1/chat/completions`:
//!   - big requests (word count > BIG_THRESHOLD_WORDS): `cache_prompt=true`,
//!     restore candidate lookup, save + meta after completion
//!   - small requests: no restore, no save/meta on the non-stream path
//!   - stream: a background reader pumps raw SSE bytes through a bounded
//!     channel; in its "finally" it always saves + writes meta + releases
//!     the slot (same as the Python reader task). If the client drops its
//!     connection, the reader aborts the in-flight request to llama-server
//!     (drops the upstream stream, closing the TCP connection — the backend
//!     has no cancel endpoint) before running the cleanup
//!   - non-stream: the full response is read before it is sent; if the
//!     client drops its connection meanwhile, hyper drops the in-flight
//!     handler future, which drops the LlamaClient future and aborts the
//!     upstream request to llama-server the same way
//!   - 503 when no slot could be acquired within the 300 s timeout
//!
//! Concurrent-request coalescing (`COALESCE_REQUESTS`): when enabled,
//! concurrent requests with the same cache key and identical generation
//! parameters (the request body minus `messages`) form one group; the first
//! request ("leader") runs the pipeline above and every other request
//! ("follower") waits for the leader and receives its result. Followers
//! never touch slots, backends or meta files. See the `coalesce` module.

use crate::coalesce::{Flight, SharedOutcome, SingleFlight};
use crate::config::Config;
use crate::hashing;
use crate::llama_client::{BackendError, JsonChat, LlamaBackend};
use crate::slot_manager::{AcquireTimeout, GSlot, SlotGuard, SlotManager};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// Shared application state (Python: `app.state`).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub clients: Vec<Arc<dyn LlamaBackend>>,
    pub sm: Arc<SlotManager>,
    /// In-flight coalesced request groups (active when
    /// `config.coalesce_requests` is enabled).
    pub sf: Arc<SingleFlight>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(state)
}

async fn models(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(json!({ "data": [{ "id": state.config.model_id }] }))
}

async fn chat(State(state): State<AppState>, req: axum::http::Request<Body>) -> Response {
    let t0 = Instant::now();
    let raw = match to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return json_error(400, format!("reading body failed: {e}")),
    };
    let body: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => return json_error(422, format!("invalid json body: {e}")),
    };
    if !body.is_object() {
        return json_error(422, "request body must be a JSON object");
    }

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let client_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&state.config.model_id)
        .to_string();

    if state.clients.is_empty() {
        return json_error(503, "no backends configured");
    }
    // model_id is taken from the first backend (used for cache keys only)
    let backend_model_id = state.clients[0].get_model_id().await;

    let prefix = hashing::raw_prefix(&messages);
    let full_for_key = format!("{}\n{}", backend_model_id, prefix);
    let key = hashing::prefix_key_sha256(&full_for_key);
    let blocks = hashing::block_hashes_from_text(&prefix, state.config.words_per_block);
    let n_words = hashing::words_from_text(&prefix).len();
    let is_big = n_words > state.config.big_threshold_words;

    // Concurrent-request coalescing (`COALESCE_REQUESTS`): concurrent
    // requests with the same cache key share one backend call — the first
    // becomes the group's leader, the rest wait for its result. Grouping is
    // based solely on the cache key (message content + backend model id):
    // generation parameters do NOT break groups, so followers receive the
    // leader's generated result (leader's sampling params, max_tokens, and
    // stream mode apply to the whole group).
    let grouped: Option<(String, Arc<Flight>)> = if state.config.coalesce_requests {
        let fk = key.clone();
        let (flight, leader) = state.sf.enter(&fk);
        if leader {
            tracing::info!("coalesce_lead key={} group={}", short16(&key), short16(&fk));
            Some((fk, flight))
        } else {
            tracing::info!("coalesce_join key={} group={}", short16(&key), short16(&fk));
            return follower_response(
                flight,
                &client_model,
                stream,
                state.config.stream_queue_size,
            )
            .await;
        }
    } else {
        None
    };

    if let Some((fk, flight)) = grouped {
        // Leader. Run the pipeline in a spawned task so a disconnect of the
        // leader's own client cannot strand the group's followers: the task
        // always runs to completion and finishes the flight.
        let state2 = state.clone();
        let body2 = body.clone();
        let client_model2 = client_model.clone();
        let backend_model_id2 = backend_model_id.clone();
        let key2 = key.clone();
        let blocks2 = blocks.clone();
        let prefix2 = prefix.clone();
        let grouped2 = Some((fk.clone(), Arc::clone(&flight)));
        let handle = tokio::spawn(async move {
            run_pipeline(
                &state2,
                &body2,
                &client_model2,
                stream,
                &backend_model_id2,
                &key2,
                &blocks2,
                &prefix2,
                is_big,
                n_words,
                t0,
                grouped2,
            )
            .await
        });
        let (resp, outcome) = match handle.await {
            Ok(t) => t,
            Err(e) => {
                // The leader task died (panic/abort): unblock the group.
                flight.finish(SharedOutcome::Json(
                    500,
                    json!({ "error": format!("coalesced leader failed: {e}") }),
                ));
                state.sf.forget(&fk);
                return json_error(500, format!("internal error: coalesced leader failed: {e}"));
            }
        };
        if let Some(o) = outcome {
            flight.finish(o);
            state.sf.forget(&fk);
        }
        // `None` outcome: streaming leader — its reader task finishes the
        // flight after the stream ends.
        return resp;
    }

    // Coalescing disabled: run inline (cancellation semantics unchanged).
    let (resp, _outcome) = run_pipeline(
        &state,
        &body,
        &client_model,
        stream,
        &backend_model_id,
        &key,
        &blocks,
        &prefix,
        is_big,
        n_words,
        t0,
        None,
    )
    .await;
    resp
}

/// The per-request pipeline (restore lookup -> slot acquire -> backend call
/// -> save/meta -> release). Returns the response and, when coalescing is
/// active, the outcome shared with the group's followers (`None` for a
/// streaming leader: its reader task finishes the flight after the stream
/// ends).
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    state: &AppState,
    body: &Value,
    client_model: &str,
    stream: bool,
    backend_model_id: &str,
    key: &str,
    blocks: &[String],
    prefix: &str,
    is_big: bool,
    n_words: usize,
    t0: Instant,
    grouped: Option<(String, Arc<Flight>)>,
) -> (Response, Option<SharedOutcome>) {
    let mut restore_key: Option<String> = None;
    if is_big {
        match hashing::find_best_restore_candidate(
            &state.config.meta_dir,
            blocks,
            state.config.words_per_block,
            state.config.lcp_th,
            backend_model_id,
        ) {
            Some((k, ratio)) => {
                tracing::info!(
                    "restore_candidate basename={} ratio={ratio:.3}",
                    short16(&k)
                );
                restore_key = Some(k);
            }
            None => tracing::info!("restore_candidate none"),
        }
    } else {
        tracing::info!(
            "small_request n_words={n_words} threshold={}",
            state.config.big_threshold_words
        );
    }

    tracing::info!(
        "before_acquire is_big={is_big} restore_key={}",
        restore_key.as_ref().map(|k| short16(k)).unwrap_or_default()
    );

    let mut guard = match state.sm.acquire(restore_key.as_deref()).await {
        Ok(g) => g,
        Err(AcquireTimeout) => {
            tracing::error!(
                "acquire_timeout is_big={is_big} restore_key={}",
                restore_key.as_ref().map(|k| short16(k)).unwrap_or_default()
            );
            let (resp, o) = err_outcome(503, "all slots busy, please retry later");
            return (resp, Some(o));
        }
    };
    let mut g: GSlot = guard.slot;
    tracing::info!("after_acquire g={g} restored={:?}", guard.restored);

    // Up to 2 attempts: a connection-level failure (backend unreachable)
    // retries once on a slot of a *different* backend before the client
    // sees an error (deviation from Python, which returned the 500).
    for attempt in 0..2 {
        let client = Arc::clone(&state.clients[g.be]);
        let out_body = build_out_body(body, client_model, is_big, g);

        tracing::info!(
            "dispatch be={} slot={} is_big={is_big} restored={:?} attempt={attempt} backend_model_id={backend_model_id}",
            g.be,
            g.slot,
            guard.restored
        );

        if stream {
            let sc = match client.chat_stream(&out_body, Some(g.slot)).await {
                Ok(sc) => sc,
                Err(e) if matches!(e, BackendError::Other(_)) => {
                    match retry_after_failure(
                        state,
                        key,
                        g,
                        guard,
                        &restore_key,
                        attempt,
                        &e.to_string(),
                    )
                    .await
                    {
                        Some(g2) => {
                            g = g2.slot;
                            guard = g2;
                            continue;
                        }
                        None => {
                            let (resp, o) = err_outcome(500, format!("backend unreachable: {e}"));
                            return (resp, Some(o));
                        }
                    }
                }
                Err(e) => {
                    state.sm.mark_used(g);
                    state.sm.release(guard);
                    let (resp, o) = err_outcome(500, e.to_string());
                    return (resp, Some(o));
                }
            };
            if sc.status != 200 {
                // non-2xx: drain the error body, release, pass status
                // through. The backend answered, so it is reachable.
                let mut err_txt = String::new();
                let mut s = sc.stream;
                while let Some(chunk) = s.next().await {
                    match chunk {
                        Ok(b) => err_txt.push_str(&String::from_utf8_lossy(&b)),
                        Err(_) => break,
                    }
                    if err_txt.len() > 65536 {
                        break;
                    }
                }
                state.sm.report_success(g);
                state.sm.mark_used(g);
                state.sm.release(guard);
                let status =
                    StatusCode::from_u16(sc.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let val = json!({ "error": err_txt });
                return (
                    (status, axum::Json(val.clone())).into_response(),
                    Some(SharedOutcome::Json(sc.status, val)),
                );
            }

            // Reader task: pumps SSE chunks through a bounded channel. In its
            // "finally" it always does save_after + write_meta + release (the
            // same as the Python reader, including for small requests), then
            // drops the sender so the response stream ends.
            //
            // Client-disconnect handling: the response body carries a oneshot
            // sender; when the client drops its connection to the proxy,
            // hyper drops the body, which drops the sender and wakes the
            // reader. The reader then drops the upstream stream *immediately*
            // (aborting the in-flight request to llama-server — the backend
            // has no cancel endpoint, so the closed TCP connection is the
            // cancellation) before running the cleanup.
            let (tx, rx) =
                mpsc::channel::<Result<Bytes, std::io::Error>>(state.config.stream_queue_size);
            let (client_gone_tx, mut client_gone_rx) = tokio::sync::oneshot::channel::<()>();
            let sm = Arc::clone(&state.sm);
            let cfg = Arc::clone(&state.config);
            let sf = Arc::clone(&state.sf);
            let key2 = key.to_string();
            let prefix2 = prefix.to_string();
            let blocks2: Vec<String> = blocks.to_vec();
            let model_id2 = backend_model_id.to_string();
            // Coalescing: share every SSE chunk with the group's followers
            // and finish their flight when the stream ends.
            let flight: Option<(String, Arc<Flight>)> = grouped.clone();
            if let Some((_, f)) = &flight {
                // The upstream accepted the stream: followers may now commit
                // to a 200 SSE response.
                f.mark_stream_started();
            }
            tracing::info!("stream_reader_start g={g} key={}", short16(&key2));

            tokio::spawn(async move {
                let mut s = sc.stream;
                let mut client_gone = false;
                loop {
                    tokio::select! {
                        _ = &mut client_gone_rx => {
                            // the client dropped its connection to the proxy
                            // -> stop pumping and abort the upstream request
                            tracing::warn!(
                                "stream_reader_client_gone g={g} key={} (aborting upstream request)",
                                short16(&key2)
                            );
                            client_gone = true;
                            break;
                        }
                        chunk = s.next() => match chunk {
                            None => break,
                            Some(Ok(bytes)) if !bytes.is_empty() => {
                                if let Some((_, f)) = &flight {
                                    f.append_sse(&bytes);
                                }
                                if tx.send(Ok(bytes)).await.is_err() {
                                    // client went away (body dropped) -> stop
                                    // pumping; still clean up
                                    tracing::warn!(
                                        "stream_reader_client_gone g={g} key={}",
                                        short16(&key2)
                                    );
                                    client_gone = true;
                                    break;
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                tracing::warn!(
                                    "stream_reader_error g={g} key={}: {e}",
                                    short16(&key2)
                                );
                                break;
                            }
                        },
                    }
                }
                if client_gone {
                    // Abort the request to llama-server now: dropping the
                    // stream closes the upstream TCP connection; the backend
                    // frees its slot when it notices the closed connection.
                    // The cleanup below (save + meta + release) still runs.
                    drop(s);
                }
                let ok = match sm.save_after(g, &key2).await {
                    Ok(ok) => {
                        sm.report_success(g);
                        ok
                    }
                    Err(e) => {
                        tracing::warn!("save_after_exception g={g} key={}: {e}", short16(&key2));
                        if matches!(e, BackendError::Other(_)) {
                            sm.report_failure(g);
                        }
                        sm.mark_used(g);
                        false
                    }
                };
                if let Err(e) = hashing::write_meta(
                    &cfg.meta_dir,
                    &key2,
                    &prefix2,
                    &blocks2,
                    cfg.words_per_block,
                    &model_id2,
                ) {
                    tracing::warn!("write_meta_exception key={}: {e}", short16(&key2));
                } else {
                    prune_cache(&cfg);
                }
                sm.release(guard);
                tracing::info!("stream_reader_done g={g} key={} saved={ok}", short16(&key2));
                // Finish the coalesced group: followers receive the full
                // stream (whatever was produced before the client went away
                // or the upstream ended).
                if let Some((fk, f)) = &flight {
                    let full = f.sse_bytes();
                    f.finish(SharedOutcome::Sse(full));
                    sf.forget(fk);
                }
                // tx is dropped here -> the response stream ends
            });

            let headers = [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
                (header::CONNECTION, "keep-alive"),
            ];
            // bridge the mpsc receiver into a futures Stream (channel close ends
            // it). The oneshot sender rides along in the unfold state, so it is
            // dropped as soon as the client drops the response body — that drop
            // is what wakes the reader to abort the upstream request.
            let stream =
                futures::stream::unfold((rx, client_gone_tx), |(mut rx, gone_tx)| async move {
                    rx.recv().await.map(|item| (item, (rx, gone_tx)))
                });
            return ((headers, Body::from_stream(stream)).into_response(), None);
        } else {
            match client.chat_json(&out_body, Some(g.slot)).await {
                Err(BackendError::HttpStatus { status, body }) => {
                    // mirrors the Python `raise_for_status` path -> 500. The
                    // backend answered, so it is reachable.
                    state.sm.report_success(g);
                    state.sm.mark_used(g);
                    state.sm.release(guard);
                    let body: String = body.chars().take(512).collect();
                    let (resp, o) =
                        err_outcome(500, format!("provider returned HTTP {status}: {body}"));
                    return (resp, Some(o));
                }
                Err(e) if matches!(e, BackendError::Other(_)) => {
                    match retry_after_failure(
                        state,
                        key,
                        g,
                        guard,
                        &restore_key,
                        attempt,
                        &e.to_string(),
                    )
                    .await
                    {
                        Some(g2) => {
                            g = g2.slot;
                            guard = g2;
                            continue;
                        }
                        None => {
                            let (resp, o) = err_outcome(500, format!("backend unreachable: {e}"));
                            return (resp, Some(o));
                        }
                    }
                }
                Err(e) => {
                    state.sm.mark_used(g);
                    state.sm.release(guard);
                    let (resp, o) = err_outcome(500, e.to_string());
                    return (resp, Some(o));
                }
                Ok(JsonChat::NonObject) => {
                    state.sm.report_success(g);
                    state.sm.mark_used(g);
                    state.sm.release(guard);
                    let (resp, o) = err_outcome(502, "provider non-JSON body");
                    return (resp, Some(o));
                }
                Ok(JsonChat::NonJson { message, raw }) => {
                    // provider answered 200 with a non-JSON / broken body (the
                    // backend is reachable). In the Python app the returned error
                    // dict is still a dict, so big requests run the same save +
                    // write-meta path before the 200.
                    state.sm.report_success(g);
                    let save_ok = if is_big {
                        match save_big(state, g, key, prefix, blocks, backend_model_id).await {
                            Ok(ok) => ok,
                            Err((resp, o)) => {
                                state.sm.release(guard);
                                return (resp, Some(o));
                            }
                        }
                    } else {
                        false
                    };
                    state.sm.release(guard);
                    tracing::info!(
                        "json_done g={g} key={} saved={save_ok} is_big={is_big} dur_ms={}",
                        short16(key),
                        t0.elapsed().as_millis()
                    );
                    let val = json!({ "object": "error", "message": message, "raw": raw });
                    return (
                        (StatusCode::OK, axum::Json(val.clone())).into_response(),
                        Some(SharedOutcome::Json(200, val)),
                    );
                }
                Ok(JsonChat::Object(out)) => {
                    state.sm.report_success(g);
                    let save_ok = if is_big {
                        match save_big(state, g, key, prefix, blocks, backend_model_id).await {
                            Ok(ok) => ok,
                            Err((resp, o)) => {
                                state.sm.release(guard);
                                return (resp, Some(o));
                            }
                        }
                    } else {
                        false
                    };
                    state.sm.release(guard);
                    tracing::info!(
                        "json_done g={g} key={} saved={save_ok} is_big={is_big} dur_ms={}",
                        short16(key),
                        t0.elapsed().as_millis()
                    );
                    return (
                        (StatusCode::OK, axum::Json(out.clone())).into_response(),
                        Some(SharedOutcome::Json(200, out)),
                    );
                }
            }
        }
    }
    unreachable!("every attempt returns or schedules the retry")
}

// ---------------------------------------------------------------------------
// Concurrent-request coalescing helpers (COALESCE_REQUESTS)
// ---------------------------------------------------------------------------

/// A JSON error response plus the shared outcome for the group's followers.
fn err_outcome(status: u16, msg: impl std::fmt::Display) -> (Response, SharedOutcome) {
    let msg = msg.to_string();
    (
        json_error(status, msg.clone()),
        SharedOutcome::Json(status, json!({ "error": msg })),
    )
}

/// Wait for the leader's terminal result.
async fn wait_outcome(flight: &Flight) -> SharedOutcome {
    loop {
        let notified = flight.notified();
        tokio::pin!(notified);
        if let Some(o) = flight.outcome() {
            return o;
        }
        notified.await;
    }
}

/// Serve a coalesced (follower) request from the leader's result: no slot
/// is acquired, no backend call is made, and no meta file is written.
async fn follower_response(
    flight: Arc<Flight>,
    client_model: &str,
    stream: bool,
    stream_queue_size: usize,
) -> Response {
    if !stream {
        let outcome = wait_outcome(&flight).await;
        return render_follower_json(&outcome, client_model);
    }
    // Stream follower: wait until the leader either starts streaming or
    // fails; then tail the live buffer or mirror the error.
    loop {
        let notified = flight.notified();
        tokio::pin!(notified);
        if let Some(o) = flight.outcome() {
            return render_stream_follower_terminal(&o, client_model);
        }
        if flight.stream_started() {
            break;
        }
        notified.await;
    }
    let (tx, rx) = mpsc::channel(stream_queue_size);
    let flight2 = Arc::clone(&flight);
    tokio::spawn(async move {
        let mut pos = 0usize;
        loop {
            let notified = flight2.notified();
            tokio::pin!(notified);
            let chunk = flight2.drain_sse_from(&mut pos);
            if !chunk.is_empty() && tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return; // client went away
            }
            if flight2.outcome().is_some() {
                // Final drain in case the last append raced the finish.
                let rest = flight2.drain_sse_from(&mut pos);
                if !rest.is_empty() {
                    let _ = tx.send(Ok(Bytes::from(rest))).await;
                }
                return;
            }
            notified.await;
        }
    });
    sse_response_simple(rx)
}

/// Build the body sent to llama.cpp (same fields as app.py), pinned to
/// slot `g`.
fn build_out_body(body: &Value, client_model: &str, is_big: bool, g: GSlot) -> Value {
    let mut out_body = body.clone();
    out_body["model"] = json!(client_model);
    out_body["cache_prompt"] = json!(is_big);
    out_body["n_keep"] = json!(-1);
    let mut opts = out_body
        .get("options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    opts.insert("slot_id".into(), json!(g.slot));
    opts.insert("id_slot".into(), json!(g.slot));
    opts.insert("n_keep".into(), json!(-1));
    opts.insert("cache_prompt".into(), json!(is_big));
    out_body["options"] = Value::Object(opts);
    out_body
}

/// Connection-level failure on slot `g`: log it, trip the circuit breaker,
/// mark the slot used, release it, and — on the first attempt — acquire a
/// slot of a *different* backend for the retry (the restore key is
/// re-attempted there). `None` when the retry budget is spent or no other
/// backend has an eligible slot (single-backend deployment, or every other
/// backend cooling down).
async fn retry_after_failure(
    state: &AppState,
    key: &str,
    g: GSlot,
    guard: SlotGuard,
    restore_key: &Option<String>,
    attempt: usize,
    err: &str,
) -> Option<SlotGuard> {
    tracing::warn!("chat_error g={g} key={}: {err}", short16(key));
    state.sm.report_failure(g);
    state.sm.mark_used(g);
    state.sm.release(guard);
    if attempt >= 1 {
        return None;
    }
    let g2 = state
        .sm
        .acquire_excluding(g.be, restore_key.as_deref())
        .await
        .ok()?;
    tracing::info!("chat_retry from_g={g} to_g={} attempt=1", g2.slot);
    Some(g2)
}

/// Runs the big-request `save_after` + `write_meta` sequence.
///
/// Returns the save status on success, or a 500 response (plus its shared
/// coalescing outcome) when either step fails (mirrors the Python
/// `try / except` around `sm.save_after` + `hs.write_meta`, which bubbles
/// up to the outer 500 handler).
async fn save_big(
    state: &AppState,
    g: GSlot,
    key: &str,
    prefix: &str,
    blocks: &[String],
    backend_model_id: &str,
) -> Result<bool, (Response, SharedOutcome)> {
    let ok = state.sm.save_after(g, key).await.map_err(|e| {
        if matches!(e, BackendError::Other(_)) {
            tracing::warn!("save_error g={g} key={}: {e}", short16(key));
            state.sm.report_failure(g);
        }
        state.sm.mark_used(g);
        let msg = e.to_string();
        (
            json_error(500, msg.clone()),
            SharedOutcome::Json(500, json!({ "error": msg })),
        )
    })?;
    hashing::write_meta(
        &state.config.meta_dir,
        key,
        prefix,
        blocks,
        state.config.words_per_block,
        backend_model_id,
    )
    .map_err(|e| {
        let msg = e.to_string();
        (
            json_error(500, msg.clone()),
            SharedOutcome::Json(500, json!({ "error": msg })),
        )
    })?;
    prune_cache(&state.config);
    Ok(ok)
}

/// Bounded LRU cache pruning (deviation from Python, which never prunes).
///
/// After a successful save: if `META_DIR` holds more than `meta_max` meta
/// files, the oldest ones are removed — together with the KV files that
/// bear the same names in the configured `--slot-save-path` directories
/// (per-backend `slot_save_path` entries, or `SLOT_SAVE_PATH` for the
/// single-backend fallback). Pruning only ever deletes files, and a later
/// restore of a pruned key simply fails and falls back to a full prefill.
fn prune_cache(config: &Config) {
    if config.meta_max == 0 {
        return;
    }
    let pruned = hashing::prune_meta(&config.meta_dir, config.meta_max);
    if pruned.is_empty() {
        return;
    }
    for key in &pruned {
        for dir in config.slot_save_dirs() {
            // NotFound is normal: the KV file may only exist in some
            // backends' directories (or nowhere if none is configured).
            if let Err(e) = std::fs::remove_file(dir.join(key)) {
                tracing::debug!(
                    "prune_kv_fail dir={} key={}: {e}",
                    dir.display(),
                    short16(key)
                );
            }
        }
    }
}

fn json_error(status: u16, msg: impl Into<String>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(json!({ "error": msg.into() }))).into_response()
}

fn short16(s: &str) -> String {
    s.chars().take(16).collect()
}

/// Render the leader's result for a non-streaming follower.
fn render_follower_json(outcome: &SharedOutcome, client_model: &str) -> Response {
    match outcome {
        SharedOutcome::Json(status, value) => {
            let mut v = value.clone();
            if *status == 200 {
                // Echo the model name the follower asked for.
                if let Some(o) = v.as_object_mut() {
                    o.insert("model".into(), json!(client_model));
                }
            }
            (
                StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                axum::Json(v),
            )
                .into_response()
        }
        SharedOutcome::Sse(bytes) => {
            // Unreachable in practice (the stream flag is part of the group
            // key): assemble a completion from the stream defensively.
            (
                StatusCode::OK,
                axum::Json(sse_to_completion(bytes, client_model)),
            )
                .into_response()
        }
        SharedOutcome::Raw(status, _mime, bytes) => raw_response(*status, bytes),
    }
}

/// Render the leader's terminal result for a streaming follower (the leader
/// finished, or failed, before this follower started tailing).
fn render_stream_follower_terminal(outcome: &SharedOutcome, client_model: &str) -> Response {
    match outcome {
        // The leader already finished: replay the recorded stream.
        SharedOutcome::Sse(bytes) => sse_response_bytes(bytes),
        SharedOutcome::Json(status, value) => {
            if *status == 200 {
                // Defensive: a stream group normally ends with `Sse`.
                sse_response_bytes(&sse_from_completion(value, client_model))
            } else {
                // The leader failed before streaming: mirror the error.
                (
                    StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    axum::Json(value.clone()),
                )
                    .into_response()
            }
        }
        SharedOutcome::Raw(status, _mime, bytes) => raw_response(*status, bytes),
    }
}

fn raw_response(status: u16, bytes: &[u8]) -> Response {
    let mut resp = Response::new(Body::from(bytes.to_vec()));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp
}

/// SSE response for a follower's live tail channel.
fn sse_response_simple(rx: mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Response {
    let headers = [
        (header::CONTENT_TYPE, "text/event-stream"),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONNECTION, "keep-alive"),
    ];
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (headers, Body::from_stream(stream)).into_response()
}

/// SSE response replaying already-recorded stream bytes.
fn sse_response_bytes(bytes: &[u8]) -> Response {
    let headers = [
        (header::CONTENT_TYPE, "text/event-stream"),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONNECTION, "keep-alive"),
    ];
    let b = Bytes::copy_from_slice(bytes);
    let stream = futures::stream::once(async move { Ok::<Bytes, std::io::Error>(b) });
    (headers, Body::from_stream(stream)).into_response()
}

/// Assemble a non-streaming completion object from raw SSE bytes (defensive
/// path: only used if a non-streaming follower ever sees a stream outcome).
fn sse_to_completion(bytes: &[u8], client_model: &str) -> Value {
    let mut content = String::new();
    let mut meta: Option<(String, u64)> = None;
    let mut finish: Option<String> = None;
    let mut usage: Option<Value> = None;
    for line in String::from_utf8_lossy(bytes).lines() {
        let data = match line.trim().strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if meta.is_none()
            && let (Some(id), Some(created)) = (
                v.get("id").and_then(Value::as_str),
                v.get("created").and_then(Value::as_u64),
            )
        {
            meta = Some((id.to_string(), created));
        }
        if let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            if let Some(delta) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
            {
                content.push_str(delta);
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                finish = Some(fr.to_string());
            }
        }
        if let Some(u) = v.get("usage")
            && !u.is_null()
        {
            usage = Some(u.clone());
        }
    }
    let (id, created) = meta.unwrap_or_else(|| ("chatcmpl-coalesced".to_string(), 0));
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": client_model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": finish.unwrap_or_else(|| "stop".to_string()),
        }],
        "usage": usage.unwrap_or_else(|| json!({
            "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0
        })),
    })
}

/// Synthesize a minimal SSE stream (one chunk + `[DONE]`) from a
/// non-streaming completion (defensive path).
fn sse_from_completion(value: &Value, client_model: &str) -> Vec<u8> {
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let finish = value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let id = value
        .get("id")
        .cloned()
        .unwrap_or_else(|| json!("chatcmpl-coalesced"));
    let created = value.get("created").cloned().unwrap_or_else(|| json!(0));
    let usage = value.get("usage").cloned().unwrap_or(Value::Null);
    let chunk = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": client_model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": content },
            "finish_reason": finish,
        }],
        "usage": usage,
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes()
}
