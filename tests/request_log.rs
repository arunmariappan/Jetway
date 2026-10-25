//! Phase 2: one stored record per exchange, bodies, rollups, Prometheus and spans.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::routing::post;
use common::{Stack, chat, script};
use jetway::config::BodyPolicy;
use jetway::store::{ListFilter, Store};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;
use serde_json::json;

async fn rows(stack: &Stack) -> Vec<jetway::store::ExchangeRow> {
    stack.running().store.list(ListFilter { limit: 100, ..ListFilter::default() }).await.unwrap()
}

#[tokio::test]
async fn every_outcome_is_one_row_with_the_upstream_token_counts() {
    let stack = Stack::start_with(|_| {}).await;
    // ok, streamed and not
    stack.post("/a/pp/api/chat", chat(true)).send().await.unwrap().bytes().await.unwrap();
    stack.post("/a/pp/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    // an upstream error status
    let (name, value) = script(r#"{"status":500}"#);
    stack.post("/api/chat", chat(false)).header(name, value).send().await.unwrap().bytes().await.unwrap();
    // a mid-stream failure
    let (name, value) = script(r#"{"drop_after":2}"#);
    stack.post("/api/chat", chat(true)).header(name, value).send().await.unwrap().bytes().await.unwrap();
    // rejected
    stack.post("/api/nope", json!({})).send().await.unwrap().bytes().await.unwrap();
    // a client that leaves
    let (name, value) = script(r#"{"gap_ms":20,"chunks":500}"#);
    let mut response = stack.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    response.chunk().await.unwrap();
    drop(response);
    stack.settle(6).await;

    let rows = rows(&stack).await;
    assert_eq!(rows.len(), 6);
    let outcomes: Vec<(&str, u16)> = rows.iter().map(|r| (r.outcome.as_str(), r.status)).collect();
    assert_eq!(
        outcomes,
        [("ok", 200), ("ok", 200), ("ok", 500), ("upstream_error", 200), ("rejected", 404), ("client_cancelled", 200)]
    );
    for row in &rows[..2] {
        assert_eq!(row.app, "pp");
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens, row.done_reason.as_deref()),
            (Some(18), Some(10), Some("stop"))
        );
        assert_eq!(row.model.as_deref(), Some("qwen3.5:4b"));
        assert!(row.first_token_us.is_some() && row.done_us.is_some());
    }
    assert_eq!(rows[0].stream, Some(true));
    assert_eq!(rows[1].stream, Some(false));
    assert_eq!(rows[1].cached_prompt_tokens, Some(14));
    assert_eq!(rows[2].error.as_deref(), Some("scripted failure"), "the upstream's error message is kept");
    assert_eq!(rows[4].error.as_deref(), Some("Jetway doesn't forward /api/nope"));
}

#[tokio::test]
async fn bodies_are_stored_compressed_and_read_back() {
    let stack = Stack::start_with(|_| {}).await;
    let sent = chat(true);
    let received = stack.post("/api/chat", sent.clone()).send().await.unwrap().bytes().await.unwrap();
    stack.settle(1).await;
    let id = rows(&stack).await[0].id.clone();
    let (_, bodies) = stack.running().store.get(id).await.unwrap().unwrap();
    let bodies = bodies.unwrap();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&bodies.request).unwrap(), sent);
    assert_eq!(bodies.response, received);
}

#[tokio::test]
async fn the_errors_policy_keeps_only_failed_bodies() {
    let stack = Stack::start_with(|c| c.store.bodies = BodyPolicy::Errors).await;
    stack.post("/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    stack.post("/api/nope", json!({})).send().await.unwrap().bytes().await.unwrap();
    stack.settle(2).await;
    let rows = rows(&stack).await;
    let store = &stack.running().store;
    assert!(store.get(rows[0].id.clone()).await.unwrap().unwrap().1.is_none());
    assert!(store.get(rows[1].id.clone()).await.unwrap().unwrap().1.is_some());
}

#[tokio::test]
async fn prometheus_counts_requests_and_tokens() {
    let stack = Stack::start_with(|_| {}).await;
    for _ in 0..3 {
        stack.post("/a/pp/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    }
    stack.settle(3).await;
    let text = stack.client.get(format!("{}/metrics", stack.admin_url)).send().await.unwrap().text().await.unwrap();
    assert!(text.contains(r#"jetway_requests_total{app="pp",model="qwen3.5:4b",outcome="ok"} 3"#), "{text}");
    assert!(text.contains(r#"jetway_tokens_total{app="pp",model="qwen3.5:4b",kind="completion"} 30"#), "{text}");
    assert!(text.contains(r#"jetway_request_duration_seconds_count{app="pp",model="qwen3.5:4b"} 3"#), "{text}");
    assert!(text.contains("jetway_in_flight 0"));
    assert_eq!(
        stack.client.get(format!("{}/healthz", stack.admin_url)).send().await.unwrap().text().await.unwrap(),
        "ok"
    );
}

#[tokio::test]
async fn closed_minutes_become_rollup_rows() {
    let stack = Stack::start_with(|_| {}).await;
    for _ in 0..4 {
        stack.post("/a/pp/api/chat", chat(true)).send().await.unwrap().bytes().await.unwrap();
    }
    stack.settle(4).await;
    let running = stack.running();
    // Pretend the minute has closed.
    let rows = running.metrics.take_closed(jiff::Timestamp::now().as_millisecond() + 60_000);
    assert_eq!(rows.len(), 1);
    running.store.write_rollups(rows);
    running.store.flush().await.unwrap();
    let stored = running.store.rollups_since(0).await.unwrap();
    assert_eq!((stored.len(), stored[0].requests, stored[0].ok, stored[0].completion_tokens), (1, 4, 4, 40));
    let latency: hdrhistogram::Histogram<u64> =
        hdrhistogram::serialization::Deserializer::new().deserialize(&mut &stored[0].latency[..]).unwrap();
    assert_eq!(latency.len(), 4);
}

#[tokio::test]
async fn a_busy_database_drops_records_instead_of_blocking_requests() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.db");
    let store = Arc::new(Store::open_with_capacity(&path, 4).unwrap());
    // Another connection holds the write lock, so the writer stalls in its busy timeout.
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    for i in 0..50 {
        let row = jetway::store::ExchangeRow { id: format!("{i:026}"), outcome: "ok".into(), ..Default::default() };
        store.record(row, None);
    }
    let dropped = store.counters.dropped.load(std::sync::atomic::Ordering::Relaxed);
    // The writer may already hold a batch when the lock stalls it; everything beyond the queue is dropped.
    assert!(dropped >= 30, "dropped {dropped}");
    blocker.execute_batch("COMMIT").unwrap();
    store.flush().await.unwrap();
    assert_eq!(store.count().await.unwrap() + dropped, 50);
}

#[tokio::test]
async fn migrations_are_idempotent_and_prune_cascades_to_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    assert_eq!(Store::migrate_only(&path).unwrap(), 1);
    assert_eq!(Store::migrate_only(&path).unwrap(), 1);

    let store = Store::open(&path).unwrap();
    for (i, at) in [(1, 1_000), (2, 5_000)] {
        let row = jetway::store::ExchangeRow {
            id: format!("id{i}"),
            started_at_ms: at,
            outcome: "ok".into(),
            ..Default::default()
        };
        let bodies = jetway::store::Bodies { request: Bytes::from_static(b"{}"), response: Bytes::from_static(b"{}") };
        store.record(row, Some(bodies));
    }
    store.flush().await.unwrap();
    assert_eq!(store.prune(2_000, i64::MIN).await.unwrap(), (1, 0));
    let bodies: i64 = store.read(|c| c.query_row("SELECT COUNT(*) FROM bodies", [], |r| r.get(0))).await.unwrap();
    assert_eq!(bodies, 1, "the pruned exchange's bodies went with it");
}

#[tokio::test]
async fn spans_join_the_callers_trace_and_reach_the_collector() {
    // A minimal OTLP/HTTP collector.
    let received: Arc<Mutex<Vec<ExportTraceServiceRequest>>> = Arc::default();
    let sink = Arc::clone(&received);
    let collector = Router::new().route(
        "/v1/traces",
        post(move |body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push(ExportTraceServiceRequest::decode(body).unwrap());
                ""
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let collector_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, collector).await });

    let stack = Stack::start_with(|c| c.otel.endpoint = collector_url).await;
    let caller = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    stack
        .post("/a/pp/api/chat", chat(false))
        .header("traceparent", caller)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    stack.settle(1).await;

    let mut spans = Vec::new();
    for _ in 0..60 {
        spans = received
            .lock()
            .unwrap()
            .iter()
            .flat_map(|r| r.resource_spans.clone())
            .flat_map(|r| r.scope_spans)
            .flat_map(|s| s.spans)
            .collect();
        if spans.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(spans.len(), 2, "a server span and a client span");
    let (server, client) = (&spans[0], &spans[1]);
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    assert_eq!(hex(&server.trace_id), "0af7651916cd43dd8448eb211c80319c");
    assert_eq!(hex(&server.parent_span_id), "b7ad6b7169203331");
    assert_eq!(client.parent_span_id, server.span_id);
    assert_eq!(client.name, "chat qwen3.5:4b");
    let attribute = |name: &str| {
        client.attributes.iter().find(|a| a.key == name).and_then(|a| a.value.clone()).and_then(|v| v.value)
    };
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    assert_eq!(attribute("gen_ai.usage.output_tokens"), Some(Value::IntValue(10)));
    assert_eq!(attribute("gen_ai.request.model"), Some(Value::StringValue("qwen3.5:4b".into())));

    // The upstream saw Jetway's client span as its parent.
    let (_, _, _, headers, _) = stack.fake.events.requests().pop().unwrap();
    let upstream = headers["traceparent"].to_str().unwrap();
    assert_eq!(upstream, format!("00-0af7651916cd43dd8448eb211c80319c-{}-01", hex(&client.span_id)));
    assert!(
        server.start_time_unix_nano <= client.start_time_unix_nano
            && client.end_time_unix_nano <= server.end_time_unix_nano
    );
}
