//! Phase 1: the streaming pass-through against the fake Ollama.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{Harness, chat, script};
use jetway::proxy::record::Outcome;
use serde_json::json;
use tokio::time::Instant;

/// Reads a streamed body, noting when each byte count was reached.
async fn read_timed(mut response: reqwest::Response) -> (Vec<u8>, Vec<(Instant, usize)>) {
    let mut body = Vec::new();
    let mut marks = Vec::new();
    while let Some(chunk) = response.chunk().await.unwrap() {
        body.extend_from_slice(&chunk);
        marks.push((Instant::now(), body.len()));
    }
    (body, marks)
}

#[tokio::test]
async fn streamed_chunks_arrive_when_sent() {
    let h = Harness::start().await;
    let (name, value) = script(r#"{"gap_ms":100,"chunks":4}"#);
    let response = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    assert_eq!(response.headers()["content-type"], "application/x-ndjson");
    let (_, marks) = read_timed(response).await;

    let id = h.fake.events.requests()[0].0;
    let sent = h.fake.events.chunks_sent(id);
    assert_eq!(sent.len(), 5);
    let mut end = 0;
    for (sent_at, bytes) in sent {
        end += bytes.len();
        let (received_at, _) = marks.iter().find(|(_, len)| *len >= end).copied().unwrap();
        let delay = received_at.duration_since(sent_at);
        assert!(delay < Duration::from_millis(5), "chunk ending at byte {end} arrived {delay:?} after it was sent");
    }
}

#[tokio::test]
async fn bodies_are_forwarded_byte_for_byte() {
    let h = Harness::start().await;
    let cases = [
        ("/api/chat", chat(true)),
        ("/api/chat", chat(false)),
        ("/api/chat", json!({"model": "qwen3.5:4b", "messages": [], "think": true})),
        ("/api/generate", json!({"model": "qwen3.5:4b", "prompt": "hi"})),
        ("/api/generate", json!({"model": "qwen3.5:4b", "prompt": "hi", "stream": false})),
        ("/v1/chat/completions", json!({"model": "qwen3.5:4b", "messages": [], "stream": true})),
        ("/v1/chat/completions", json!({"model": "qwen3.5:4b", "messages": []})),
        ("/v1/completions", json!({"model": "qwen3.5:4b", "prompt": "hi", "stream": true})),
        ("/api/embed", json!({"model": "qwen3.5:4b", "input": ["a", "b"]})),
        ("/v1/embeddings", json!({"model": "qwen3.5:4b", "input": "a"})),
        ("/api/show", json!({"model": "qwen3.5:4b"})),
    ];
    for (path, body) in cases {
        let direct = h.direct(path, body.clone()).send().await.unwrap();
        let direct_type = direct.headers()["content-type"].clone();
        let direct = direct.bytes().await.unwrap();
        let through = h.post(path, body.clone()).send().await.unwrap();
        assert_eq!(through.status(), 200, "{path} {body}");
        assert_eq!(through.headers()["content-type"], direct_type, "{path} {body}");
        assert_eq!(through.bytes().await.unwrap(), direct, "{path} {body}");
    }
    for path in ["/api/tags", "/api/ps", "/api/version", "/v1/models", "/"] {
        let direct = h.client.get(format!("{}{path}", h.fake.url())).send().await.unwrap().bytes().await.unwrap();
        let through = h.client.get(format!("{}{path}", h.url)).send().await.unwrap().bytes().await.unwrap();
        assert_eq!(through, direct, "{path}");
    }
    let head = h.client.head(format!("{}/", h.url)).send().await.unwrap();
    assert_eq!(head.status(), 200);
    let records = h.records.wait_for(17).await;
    assert!(
        records.iter().all(|r| r.outcome == Outcome::Ok),
        "{:#?}",
        records.iter().map(|r| (&r.exchange.path, r.outcome)).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn lines_split_across_chunks_pass_unchanged() {
    let h = Harness::start().await;
    let (name, value) = script(r#"{"split_lines":true,"gap_ms":5}"#);
    let direct = h.direct("/api/chat", chat(true)).send().await.unwrap().bytes().await.unwrap();
    let through = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(through, direct);
    let id = h.fake.events.requests()[1].0;
    assert!(h.fake.events.chunks_sent(id).len() > 10, "the fake should have split the lines");
}

#[tokio::test]
async fn a_client_disconnect_closes_the_upstream_connection() {
    let h = Harness::start().await;
    let (name, value) = script(r#"{"gap_ms":20,"chunks":1000}"#);
    let mut response = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    response.chunk().await.unwrap();
    response.chunk().await.unwrap();
    let left = Instant::now();
    drop(response);

    let id = h.fake.events.requests()[0].0;
    let closed = h.fake.events.wait_disconnected(id, Duration::from_secs(2)).await.expect("upstream never closed");
    let after = closed.duration_since(left);
    assert!(after < Duration::from_millis(100), "the upstream connection closed {after:?} after the client left");
    let record = &h.records.wait_for(1).await[0];
    assert_eq!(record.outcome, Outcome::ClientCancelled);
    assert_eq!(h.gateway.inflight(), 0);
}

#[tokio::test]
async fn a_client_that_leaves_before_the_first_byte_cancels_the_upstream_call() {
    let h = Harness::start().await;
    let (name, value) = script(r#"{"ttfb_ms":3000}"#);
    let request = h.post("/api/chat", chat(true)).header(name, value).timeout(Duration::from_millis(200));
    assert!(request.send().await.is_err());
    let left = Instant::now();

    let id = h.fake.events.requests()[0].0;
    let closed = h.fake.events.wait_disconnected(id, Duration::from_secs(4)).await.expect("upstream never closed");
    let after = closed.duration_since(left);
    assert!(after < Duration::from_millis(500), "the upstream call was cancelled {after:?} after the client left");
}

#[tokio::test]
async fn path_rules_and_app_identity() {
    let h = Harness::start_with(|c| {
        c.apps.insert("ops".into(), jetway::config::AppConfig { admin: true, ..Default::default() });
    })
    .await;

    let unknown = h.post("/api/nope", json!({})).send().await.unwrap();
    assert_eq!(unknown.status(), 404);
    assert_eq!(unknown.text().await.unwrap(), r#"{"error":"Jetway doesn't forward /api/nope"}"#);

    let openai_unknown = h.post("/v1/assistants", json!({})).send().await.unwrap();
    assert_eq!(openai_unknown.status(), 404);
    let body: serde_json::Value = openai_unknown.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found_error");

    let pull = h.post("/a/paperpilot/api/pull", json!({"model": "x"})).send().await.unwrap();
    assert_eq!(pull.status(), 403);
    // An admin app reaches the upstream (the fake has no /api/pull, so its 404 comes back unchanged).
    let pull = h.post("/a/ops/api/pull", json!({"model": "x"})).send().await.unwrap();
    assert_eq!(pull.status(), 404);
    assert_eq!(pull.text().await.unwrap(), "404 page not found");

    let bad = h.post("/a/Bad_Name/api/chat", chat(false)).send().await.unwrap();
    assert_eq!(bad.status(), 400);

    h.post("/a/paperpilot/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    let requests = h.fake.events.requests();
    assert_eq!(requests.last().unwrap().2, "/api/chat", "the prefix is stripped");
    let records = h.records.wait_for(6).await;
    let last = records.iter().find(|r| r.exchange.app == "paperpilot" && r.status == 200).unwrap();
    assert_eq!(last.exchange.path, "/api/chat");
}

#[tokio::test]
async fn unknown_apps_are_refused_when_required() {
    let h = Harness::start_with(|c| c.server.require_known_app = true).await;
    let response = h.post("/a/stranger/api/chat", chat(false)).send().await.unwrap();
    assert_eq!(response.status(), 403);
    let response = h.post("/api/chat", chat(false)).send().await.unwrap();
    assert_eq!(response.status(), 200, "the default app is always known");
}

#[tokio::test]
async fn header_rules() {
    let h = Harness::start().await;
    let traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let response = h
        .post("/api/chat", chat(false))
        .header("authorization", "Bearer jw-secret")
        .header("x-jetway-app", "opsdesk")
        .header("traceparent", traceparent)
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert!(response.headers().contains_key("x-jetway-id"));
    response.bytes().await.unwrap();
    let (_, _, _, headers, _) = h.fake.events.requests().pop().unwrap();
    assert_eq!(headers["traceparent"], traceparent);
    for dropped in ["authorization", "x-jetway-app", "accept-encoding"] {
        assert!(!headers.contains_key(dropped), "{dropped} reached the upstream");
    }
    assert_eq!(h.records.wait_for(1).await[0].exchange.app, "opsdesk");
}

#[tokio::test]
async fn upstream_down_fails_fast_in_each_protocol() {
    let h = Harness::start_with(|c| c.upstream.url = "http://127.0.0.1:9".into()).await;
    let started = Instant::now();
    let response = h.post("/api/chat", chat(true)).send().await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(response.status(), 502);
    assert_eq!(response.text().await.unwrap(), r#"{"error":"Ollama isn't reachable at http://127.0.0.1:9"}"#);
    // A refused connection to 127.0.0.1 is immediate on Linux; Windows reports it only after about 2 s of retries.
    let limit = if cfg!(windows) { Duration::from_millis(2500) } else { Duration::from_millis(50) };
    assert!(elapsed < limit, "took {elapsed:?}");

    let response = h.post("/v1/chat/completions", json!({"messages": []})).send().await.unwrap();
    assert_eq!(response.status(), 502);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "upstream_unavailable");
    let records = h.records.wait_for(2).await;
    assert!(records.iter().all(|r| r.outcome == Outcome::UpstreamError));
}

#[tokio::test]
async fn first_byte_timeout() {
    let h = Harness::start_with(|c| c.upstream.first_byte_timeout_seconds = 1).await;
    let (name, value) = script(r#"{"ttfb_ms":3000}"#);
    let response = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    assert_eq!(response.status(), 504);
    assert_eq!(response.text().await.unwrap(), r#"{"error":"no response from the upstream within 1 s"}"#);
    assert_eq!(h.records.wait_for(1).await[0].outcome, Outcome::Timeout);
}

#[tokio::test]
async fn idle_timeout_ends_the_stream_with_an_error_line() {
    let h = Harness::start_with(|c| c.upstream.idle_timeout_seconds = 1).await;
    let (name, value) = script(r#"{"hang_after":2}"#);
    let response = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    let body = response.text().await.unwrap();
    let last = body.lines().last().unwrap();
    assert_eq!(last, r#"{"error":"no data from the upstream for 1 s"}"#);
    assert_eq!(body.lines().count(), 3);
    let record = &h.records.wait_for(1).await[0];
    assert_eq!(record.outcome, Outcome::Timeout);
    assert!(record.response.ends_with(b"1 s\"}\n"), "the error line is recorded too");
}

#[tokio::test]
async fn a_failure_mid_stream_ends_with_an_error_line() {
    let h = Harness::start().await;
    let (name, value) = script(r#"{"drop_after":3,"split_lines":true}"#);
    let response = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    let body = response.text().await.unwrap();
    // Three half-lines arrived: the error goes on a new line after the partial one.
    let last = body.lines().last().unwrap();
    assert!(last.starts_with(r#"{"error":"the upstream stream failed"#), "{body}");
    assert_eq!(h.records.wait_for(1).await[0].outcome, Outcome::UpstreamError);

    let (name, value) = script(r#"{"drop_after":2}"#);
    let response = h
        .post("/v1/chat/completions", json!({"messages": [], "stream": true}))
        .header(name, value)
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    let last_event = body.trim_end().rsplit("\n\n").next().unwrap();
    assert!(last_event.starts_with(r#"data: {"error":{"#), "{body}");
}

#[tokio::test]
async fn upstream_errors_pass_through_unchanged() {
    let h = Harness::start().await;
    let (name, value) = script(r#"{"status":500}"#);
    let response = h.post("/api/chat", chat(false)).header(name, value).send().await.unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(response.text().await.unwrap(), r#"{"error":"scripted failure"}"#);
    let record = &h.records.wait_for(1).await[0];
    assert_eq!((record.status, record.outcome), (500, Outcome::Ok));
}

#[tokio::test]
async fn request_bodies_over_the_limit_are_refused() {
    let h = Harness::start_with(|c| c.server.max_request_bytes = 1024).await;
    let big = json!({"model": "qwen3.5:4b", "prompt": "x".repeat(2000)});
    let response = h.post("/api/generate", big).send().await.unwrap();
    assert_eq!(response.status(), 413);
    assert!(h.fake.events.requests().is_empty());
}

#[tokio::test]
async fn shutdown_lets_an_in_flight_stream_finish() {
    let mut h = Harness::start().await;
    let (name, value) = script(r#"{"gap_ms":50,"chunks":10}"#);
    let mut response = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    response.chunk().await.unwrap();
    h.shutdown();
    let mut lines = 1;
    while let Some(chunk) = response.chunk().await.unwrap() {
        lines += chunk.iter().filter(|&&b| b == b'\n').count();
    }
    assert_eq!(lines, 11, "the stream ran to its end");
    let server = h.server.take().unwrap();
    tokio::time::timeout(Duration::from_secs(3), server).await.unwrap().unwrap().unwrap();
    assert_eq!(h.records.wait_for(1).await[0].outcome, Outcome::Ok);
}
