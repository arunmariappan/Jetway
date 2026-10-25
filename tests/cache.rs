//! Phase 3: the exact cache and joined requests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{Harness, Stack, chat, script};
use jetway::proxy::record::Outcome;
use serde_json::{Value, json};
use tokio::time::Instant;

async fn cache_on() -> Stack {
    Stack::start_with(|c| c.cache.exact = true).await
}

fn note(f: &jetway::proxy::record::Finished) -> &'static str {
    f.exchange.cache.as_ref().map_or("none", |n| n.result.as_str())
}

#[tokio::test]
async fn a_repeated_request_is_answered_from_the_cache() {
    let stack = cache_on().await;
    let first = stack.post("/a/pp/api/chat", chat(true)).send().await.unwrap();
    assert_eq!(first.headers()["x-jetway-cache"], "miss");
    let first_body = first.bytes().await.unwrap();
    stack.records.wait_for(1).await;

    let started = Instant::now();
    let second = stack.post("/a/pp/api/chat", chat(true)).send().await.unwrap();
    let first_byte = started.elapsed();
    assert_eq!(second.headers()["x-jetway-cache"], "hit");
    assert!(second.headers().contains_key("x-jetway-source"));
    assert_eq!(second.bytes().await.unwrap(), first_body, "same stream mode: the same bytes");
    assert!(first_byte < Duration::from_millis(20), "first byte after {first_byte:?}");

    assert_eq!(stack.fake.events.requests().len(), 1, "the upstream saw one request");
    let records = stack.records.wait_for(2).await;
    assert_eq!((note(&records[0]), note(&records[1])), ("miss", "hit"));
    let hit = records[1].exchange.cache.as_ref().unwrap();
    assert_eq!((hit.saved_prompt_tokens, hit.saved_completion_tokens), (Some(18), Some(10)));
    assert_eq!(hit.source.as_deref(), Some(records[0].exchange.id.to_string().as_str()));
}

#[tokio::test]
async fn hits_are_served_in_the_other_stream_mode_in_both_protocols() {
    let stack = cache_on().await;
    // Ollama: streamed first, then asked for whole.
    stack.post("/api/chat", chat(true)).send().await.unwrap().bytes().await.unwrap();
    stack.records.wait_for(1).await;
    let whole: Value = stack.post("/api/chat", chat(false)).send().await.unwrap().json().await.unwrap();
    assert_eq!(whole["message"]["content"], "Hello there, friend! How are you?");
    assert_eq!((whole["done"].as_bool(), whole["eval_count"].as_u64()), (Some(true), Some(10)));

    // OpenAI: whole first, then streamed.
    let body = json!({"model": "qwen3.5:4b", "messages": [{"role": "user", "content": "hi"}]});
    stack.post("/v1/chat/completions", body.clone()).send().await.unwrap().bytes().await.unwrap();
    stack.records.wait_for(3).await;
    let mut streamed = body.clone();
    streamed["stream"] = json!(true);
    let response = stack.post("/v1/chat/completions", streamed).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "hit");
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let text = response.text().await.unwrap();
    assert!(text.contains(r#""content":"Hello there, friend! How are you?""#), "{text}");
    assert!(text.ends_with("data: [DONE]\n\n"));
    assert_eq!(stack.fake.events.requests().len(), 2);
}

#[tokio::test]
async fn key_rules() {
    let stack = cache_on().await;
    let base = json!({"model": "qwen3.5:4b", "messages": [{"role": "user", "content": "hi"}], "stream": false,
        "options": {"temperature": 1, "seed": 4}});
    stack.post("/api/chat", base.clone()).send().await.unwrap().bytes().await.unwrap();
    stack.records.wait_for(1).await;

    // The same request written differently is a hit.
    let reordered: Value = serde_json::from_str(
        r#"{"options":{"seed":4,"temperature":1.0},"stream":false,"messages":[{"content":"hi","role":"user"}],"model":"qwen3.5:4b","keep_alive":"5m"}"#,
    )
    .unwrap();
    let response = stack.post("/api/chat", reordered).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "hit");
    // Another app has its own entries; another temperature is another request.
    let response = stack.post("/a/other/api/chat", base.clone()).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "miss");
    let mut warmer = base.clone();
    warmer["options"]["temperature"] = json!(0.5);
    let response = stack.post("/api/chat", warmer).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "miss");
}

#[tokio::test]
async fn failures_and_cancellations_are_not_cached() {
    let stack = cache_on().await;
    let (name, value) = script(r#"{"status":500}"#);
    stack.post("/api/chat", chat(false)).header(name, value).send().await.unwrap().bytes().await.unwrap();
    let (name, value) = script(r#"{"drop_after":2}"#);
    stack.post("/api/chat", chat(true)).header(name, value).send().await.unwrap().bytes().await.unwrap();
    let (name, value) = script(r#"{"gap_ms":20,"chunks":200}"#);
    let mut response =
        stack.post("/api/generate", json!({"model": "m", "prompt": "x"})).header(name, value).send().await.unwrap();
    response.chunk().await.unwrap();
    drop(response);
    stack.records.wait_for(3).await;
    // None of them was stored: the next request for each key goes upstream. (Streamed and unstreamed chat share a
    // key, so one chat request checks both failures.)
    for (path, body) in [("/api/chat", chat(false)), ("/api/generate", json!({"model": "m", "prompt": "x"}))] {
        let response = stack.post(path, body).send().await.unwrap();
        assert_eq!(response.headers()["x-jetway-cache"], "miss", "{path}");
        response.bytes().await.unwrap();
    }
}

#[tokio::test]
async fn request_controls_and_the_sampled_policy() {
    let stack = Stack::start_with(|c| {
        c.cache.exact = true;
        c.apps.insert(
            "strict".into(),
            jetway::config::AppConfig {
                cache: jetway::config::AppCacheConfig {
                    sampled: Some(jetway::config::SampledPolicy::Skip),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
    })
    .await;
    // no-store: not used, not stored.
    let response = stack.post("/api/chat", chat(false)).header("cache-control", "no-store").send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "bypass");
    response.bytes().await.unwrap();
    stack.records.wait_for(1).await;
    assert_eq!(stack.post("/api/chat", chat(false)).send().await.unwrap().headers()["x-jetway-cache"], "miss");
    stack.records.wait_for(2).await;
    // no-cache: a fresh answer, which is stored.
    let response = stack
        .post("/api/generate", json!({"model": "m", "prompt": "a"}))
        .header("cache-control", "no-cache")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "bypass");
    response.bytes().await.unwrap();
    stack.records.wait_for(3).await;
    assert_eq!(
        stack.post("/api/generate", json!({"model": "m", "prompt": "a"})).send().await.unwrap().headers()["x-jetway-cache"],
        "hit"
    );
    // X-Jetway-Cache: off.
    let response = stack.post("/api/chat", chat(false)).header("x-jetway-cache", "off").send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "bypass");
    // An app that skips sampled requests: no seed and no temperature 0 means sampled.
    let response = stack.post("/a/strict/api/chat", chat(false)).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "off");
    let mut seeded = chat(false);
    seeded["options"] = json!({"seed": 1});
    let response = stack.post("/a/strict/api/chat", seeded).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "miss");
}

#[tokio::test]
async fn openai_hits_get_usage_when_the_client_asks() {
    let stack = cache_on().await;
    let body = json!({"model": "qwen3.5:4b", "messages": [], "stream": true});
    let first = stack.post("/v1/chat/completions", body.clone()).send().await.unwrap().text().await.unwrap();
    assert!(!first.contains("\"usage\""));
    stack.records.wait_for(1).await;
    let mut asking = body.clone();
    asking["stream_options"] = json!({"include_usage": true});
    let text = stack.post("/v1/chat/completions", asking).send().await.unwrap().text().await.unwrap();
    assert!(text.contains(r#""usage":{"prompt_tokens":18,"completion_tokens":10,"total_tokens":28}"#), "{text}");
    let again = stack.post("/v1/chat/completions", body).send().await.unwrap().text().await.unwrap();
    assert_eq!(again, first, "a client that doesn't ask gets the stream it got the first time");
}

#[tokio::test]
async fn embeddings_are_cached() {
    let stack = cache_on().await;
    let body = json!({"model": "qwen3.5:4b", "input": ["a", "b"]});
    let first = stack.post("/api/embed", body.clone()).send().await.unwrap().bytes().await.unwrap();
    stack.records.wait_for(1).await;
    let response = stack.post("/api/embed", body).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "hit");
    assert_eq!(response.bytes().await.unwrap(), first);
}

#[tokio::test]
async fn entries_survive_a_restart() {
    let mut stack = cache_on().await;
    stack.post("/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    stack.settle(1).await;
    let path = stack.dir.path().join("jetway.db");
    let running = stack.running.take().unwrap();
    running.stop();
    running.wait().await.unwrap();

    // A new Jetway on the same database answers from SQLite.
    let fake_url = stack.fake.url();
    let again = Stack::start_with(move |c| {
        c.cache.exact = true;
        c.store.path = path;
        c.upstream.url = fake_url;
    })
    .await;
    let response = again.post("/api/chat", chat(false)).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "hit");
    assert_eq!(stack.fake.events.requests().len(), 1);
}

#[tokio::test]
async fn paced_replay_takes_about_the_original_generation_time() {
    let stack = Stack::start_with(|c| {
        c.cache.exact = true;
        c.cache.replay = jetway::config::Replay::Paced;
    })
    .await;
    stack.post("/api/chat", chat(true)).send().await.unwrap().bytes().await.unwrap();
    stack.records.wait_for(1).await;
    let started = Instant::now();
    let response = stack.post("/api/chat", chat(true)).send().await.unwrap();
    assert_eq!(response.headers()["x-jetway-cache"], "hit");
    response.bytes().await.unwrap();
    // The golden answer's eval_duration is 135 ms.
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(110) && took < Duration::from_millis(400), "{took:?}");
}

#[tokio::test]
async fn cache_counts_reach_prometheus() {
    let stack = cache_on().await;
    for _ in 0..3 {
        stack.post("/a/pp/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
        stack.records.wait_for(1).await;
    }
    stack.settle(3).await;
    let text = stack.client.get(format!("{}/metrics", stack.admin_url)).send().await.unwrap().text().await.unwrap();
    assert!(text.contains(r#"jetway_cache_total{app="pp",result="hit"} 2"#), "{text}");
    assert!(text.contains(r#"jetway_cache_total{app="pp",result="miss"} 1"#), "{text}");
    assert!(text.contains(r#"jetway_cache_saved_tokens_total{app="pp"} 56"#), "{text}");
    assert!(text.contains("jetway_cache_entries 1"), "{text}");
}

// Joined requests use the in-memory harness with the cache on.
async fn joining() -> Harness {
    Harness::start_with(|c| c.cache.exact = true).await
}

#[tokio::test]
async fn identical_requests_in_flight_share_one_upstream_call() {
    let h = joining().await;
    let (name, value) = script(r#"{"gap_ms":40,"chunks":8}"#);
    let requests = (0..10).map(|_| {
        let request = h.post("/api/chat", chat(true)).header(name, value.clone());
        tokio::spawn(async move {
            let response = request.send().await.unwrap();
            let header = response.headers()["x-jetway-cache"].to_str().unwrap().to_string();
            (header, response.bytes().await.unwrap())
        })
    });
    let results: Vec<(String, bytes::Bytes)> =
        futures::future::join_all(requests).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(h.fake.events.requests().len(), 1, "one upstream call for ten identical requests");
    assert!(results.iter().all(|(_, body)| *body == results[0].1), "everyone got the same bytes");
    let joined = results.iter().filter(|(header, _)| header == "joined").count();
    assert_eq!(joined, 9);
    let records = h.records.wait_for(10).await;
    assert!(
        records.iter().all(|r| r.outcome == Outcome::Ok),
        "{:?}",
        records.iter().map(|r| r.outcome).collect::<Vec<_>>()
    );
    assert_eq!(records.iter().filter(|r| note(r) == "joined").count(), 9);
}

#[tokio::test]
async fn a_flight_goes_on_when_its_first_client_leaves() {
    let h = joining().await;
    let (name, value) = script(r#"{"gap_ms":30,"chunks":10}"#);
    let mut first = h.post("/api/chat", chat(true)).header(name, value.clone()).send().await.unwrap();
    first.chunk().await.unwrap();
    let second = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    assert_eq!(second.headers()["x-jetway-cache"], "joined");
    drop(first);
    let body = second.text().await.unwrap();
    assert_eq!(body.lines().count(), 11, "the joined client got the whole stream");
    let id = h.fake.events.requests()[0].0;
    assert!(h.fake.events.disconnected_at(id).is_none(), "the upstream call wasn't cancelled");
}

#[tokio::test]
async fn a_flight_stops_the_upstream_when_everyone_leaves() {
    let h = joining().await;
    let (name, value) = script(r#"{"gap_ms":20,"chunks":500}"#);
    let mut first = h.post("/api/chat", chat(true)).header(name, value.clone()).send().await.unwrap();
    first.chunk().await.unwrap();
    let mut second = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    second.chunk().await.unwrap();
    drop(first);
    let left = Instant::now();
    drop(second);
    let id = h.fake.events.requests()[0].0;
    let closed = h.fake.events.wait_disconnected(id, Duration::from_secs(2)).await.expect("upstream never closed");
    assert!(closed.duration_since(left) < Duration::from_millis(150));
    let records = h.records.wait_for(2).await;
    assert!(records.iter().all(|r| r.outcome == Outcome::ClientCancelled));
}

#[tokio::test]
async fn joined_requests_get_the_leaders_failure() {
    let h = Harness::start_with(|c| {
        c.cache.exact = true;
        c.upstream.first_byte_timeout_seconds = 1;
    })
    .await;
    let (name, value) = script(r#"{"ttfb_ms":3000}"#);
    let a = tokio::spawn(h.post("/api/chat", chat(true)).header(name, value.clone()).send());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = h.post("/api/chat", chat(true)).header(name, value).send().await.unwrap();
    assert_eq!(b.status(), 504);
    assert_eq!(b.headers()["x-jetway-cache"], "joined");
    assert_eq!(a.await.unwrap().unwrap().status(), 504);
    assert_eq!(h.fake.events.requests().len(), 1);
}

#[tokio::test]
async fn expired_entries_are_not_served_and_are_swept() {
    let dir = tempfile::tempdir().unwrap();
    let store = jetway::store::Store::open(&dir.path().join("c.db")).unwrap();
    let now = jiff::Timestamp::now().as_millisecond();
    let entry = |expires_ms| jetway::cache::Entry {
        app: "pp".into(),
        path: "/api/chat".into(),
        stream: false,
        content_type: "application/json".into(),
        raw: bytes::Bytes::from_static(b"{}"),
        answer: jetway::cache::answer::Answer::default(),
        source: "x".into(),
        created_ms: now - 10_000,
        expires_ms,
    };
    store.cache_put([1; 32], entry(now - 1));
    store.cache_put([2; 32], entry(now + 60_000));
    store.flush().await.unwrap();
    assert!(store.cache_get([1; 32], now).await.unwrap().is_none(), "expired");
    assert!(store.cache_get([2; 32], now).await.unwrap().is_some());
    store.prune(i64::MIN, i64::MIN).await.unwrap();
    let left: i64 = store.read(|c| c.query_row("SELECT COUNT(*) FROM cache_entries", [], |r| r.get(0))).await.unwrap();
    assert_eq!(left, 1, "the expired entry was swept");
    assert_eq!(store.cache_clear(Some("pp".into())).await.unwrap(), 1);
}
