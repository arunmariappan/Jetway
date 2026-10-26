//! Phase 9 against a fake Ollama: rate limits, budgets across a restart, the priority queue with shedding and
//! cancellation, joined requests sharing a slot, the output cap, the circuit breaker, and alerts sent to Telegram.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Harness, Stack, chat, script};
use jetway::alerts::telegram::Telegram;
use jetway::config::{AppConfig, Config, Priority};
use jetway::dev::fake_ollama::{self, Script};
use jetway::limits::Refusal;
use jetway::proxy::record::Outcome;
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn app(edit: impl FnOnce(&mut AppConfig)) -> AppConfig {
    let mut app = AppConfig::default();
    edit(&mut app);
    app
}

fn ask(content: &str) -> Value {
    json!({"model": "qwen3.5:4b", "messages": [{"role": "user", "content": content}], "stream": false})
}

fn retry_after(response: &reqwest::Response) -> u64 {
    response.headers()["retry-after"].to_str().unwrap().parse().unwrap()
}

/// The question each upstream request asked, in the order they reached the upstream.
fn upstream_questions(fake: &fake_ollama::FakeOllama) -> Vec<String> {
    fake.events
        .requests()
        .iter()
        .filter_map(|(_, _, _, _, body)| {
            let value: Value = serde_json::from_slice(body).ok()?;
            value["messages"][0]["content"].as_str().map(str::to_string)
        })
        .collect()
}

#[tokio::test]
async fn a_rate_limit_answers_429_with_retry_after_in_the_clients_format() {
    let h = Harness::start_with(|c| {
        c.apps.insert("pp".into(), app(|a| (a.requests_per_minute, a.burst) = (Some(60), Some(2))));
    })
    .await;
    for _ in 0..2 {
        assert_eq!(h.post("/a/pp/api/chat", chat(false)).send().await.unwrap().status(), 200);
    }
    let refused = h.post("/a/pp/api/chat", chat(false)).send().await.unwrap();
    assert_eq!(refused.status(), 429);
    assert_eq!(retry_after(&refused), 1, "one request a second at 60 a minute");
    let body: Value = refused.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().starts_with("rate limit: pp may send 60 requests a minute"), "{body}");

    let openai = h.post("/a/pp/v1/chat/completions", json!({"model": "m", "messages": []})).send().await.unwrap();
    assert_eq!(openai.status(), 429);
    let body: Value = openai.json().await.unwrap();
    assert!(body["error"]["message"].as_str().unwrap().starts_with("rate limit"), "{body}");
    // Model lists and other info aren't limited, and other apps have their own pace.
    assert_eq!(h.client.get(format!("{}/a/pp/api/tags", h.url)).send().await.unwrap().status(), 200);
    assert_eq!(h.post("/a/other/api/chat", chat(false)).send().await.unwrap().status(), 200);

    let records = h.records.wait_for(6).await;
    let refused = records.iter().filter(|f| f.exchange.limits.refusal == Some(Refusal::RateLimit)).count();
    assert_eq!(refused, 2);
    assert!(records.iter().filter(|f| f.status == 429).all(|f| f.outcome == Outcome::Rejected));
    assert_eq!(h.fake.events.requests().len(), 4, "three chats and the model list reached the upstream");
}

#[tokio::test]
async fn a_used_up_budget_refuses_until_it_resets_even_after_a_restart() {
    fn limits(c: &mut Config) {
        c.apps.insert("pp".into(), app(|a| a.tokens_per_day = Some(20)));
        c.budgets.time_zone = "UTC".into();
        c.cache.exact = true;
    }
    let mut stack = Stack::start_with(limits).await;
    // The golden answer uses 18 prompt and 10 output tokens: the first call overshoots a budget of 20.
    assert_eq!(stack.post("/a/pp/api/chat", ask("first")).send().await.unwrap().status(), 200);
    stack.settle(1).await;
    // A cache hit costs nothing, so it is still served.
    let hit = stack.post("/a/pp/api/chat", ask("first")).send().await.unwrap();
    assert_eq!((hit.status().as_u16(), hit.headers()["x-jetway-cache"].to_str().unwrap()), (200, "hit"));
    let refused = stack.post("/a/pp/api/chat", ask("second")).send().await.unwrap();
    assert_eq!(refused.status(), 429);
    let wait = retry_after(&refused);
    assert!(wait > 0 && wait <= 24 * 3600, "{wait}");
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["error"], "budget exceeded: pp used 28 of 20 tokens today; resets at 00:00");

    // The dashboard and the API show it used up.
    let page = stack.client.get(format!("{}/limits", stack.admin_url)).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("tokens today") && page.contains("used up"), "{page}");
    let api: Value =
        stack.client.get(format!("{}/api/limits", stack.admin_url)).send().await.unwrap().json().await.unwrap();
    assert_eq!((api["budgets"][0]["used"].as_u64(), api["budgets"][0]["level"].as_str()), (Some(28), Some("critical")));
    stack.settle(3).await;

    // A new Jetway on the same database remembers the usage.
    let path = stack.dir.path().join("jetway.db");
    let running = stack.running.take().unwrap();
    running.stop();
    running.wait().await.unwrap();
    let fake_url = stack.fake.url();
    let again = Stack::start_with(move |c| {
        limits(c);
        c.store.path = path;
        c.upstream.url = fake_url;
    })
    .await;
    let response = again.post("/a/pp/api/chat", ask("third")).send().await.unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(upstream_questions(&stack.fake), ["first"]);
}

#[tokio::test]
async fn interactive_requests_go_first_and_the_wait_is_reported() {
    let h = Harness::start_with(|c| {
        c.apps.insert("chat".into(), app(|a| a.priority = Some(Priority::Interactive)));
        c.apps.insert("bulk".into(), app(|a| a.priority = Some(Priority::Batch)));
    })
    .await;
    let (name, value) = script(r#"{"ttfb_ms":400}"#);
    let send = |path: &'static str, question: &'static str| {
        let request = h.post(path, ask(question)).header(name, value.clone());
        tokio::spawn(async move { request.send().await.unwrap() })
    };
    let first = send("/a/other/api/chat", "holds the slot");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let batch = send("/a/bulk/api/chat", "batch");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let interactive = send("/a/chat/api/chat", "interactive");

    let (first, batch, interactive) = (first.await.unwrap(), batch.await.unwrap(), interactive.await.unwrap());
    assert_eq!(upstream_questions(&h.fake), ["holds the slot", "interactive", "batch"]);
    let waited = |r: &reqwest::Response| r.headers()["x-jetway-queue-wait"].to_str().unwrap().parse::<u64>().unwrap();
    assert!(waited(&first) < 20, "{}", waited(&first));
    assert!((250..1000).contains(&waited(&interactive)), "{}", waited(&interactive));
    assert!(waited(&batch) >= waited(&interactive) + 300, "{} vs {}", waited(&batch), waited(&interactive));
    let records = h.records.wait_for(3).await;
    assert!(records.iter().all(|f| f.timings.queue_wait.is_some()));
}

#[tokio::test]
async fn a_request_that_would_wait_too_long_is_shed_and_one_whose_client_leaves_never_goes_upstream() {
    let h = Harness::start_with(|c| {
        c.apps.insert("bulk".into(), app(|a| a.priority = Some(Priority::Batch)));
        c.queue.max_wait_seconds.batch = 1;
    })
    .await;
    let (name, value) = script(r#"{"ttfb_ms":2500}"#);
    let holder = {
        let request = h.post("/api/chat", ask("holds the slot")).header(name, value);
        tokio::spawn(async move { request.send().await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = Instant::now();
    let shed = h.post("/a/bulk/api/chat", ask("shed")).send().await.unwrap();
    let took = started.elapsed();
    assert_eq!(shed.status(), 503);
    assert!(took >= Duration::from_millis(950) && took < Duration::from_millis(1500), "{took:?}");
    assert_eq!(retry_after(&shed), 30, "the aging period");
    let body: Value = shed.json().await.unwrap();
    assert_eq!(body["error"], "the upstream is busy: no slot within 1 s, the longest a batch request waits");

    // A client that gives up while queued leaves the queue; its request never reaches the upstream.
    let gave_up = h.post("/api/chat", ask("gave up")).timeout(Duration::from_millis(200)).send().await;
    assert!(gave_up.is_err());
    holder.await.unwrap().bytes().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(upstream_questions(&h.fake), ["holds the slot"]);
    assert_eq!(h.gateway.scheduler().depth().in_use, 0);

    let records = h.records.wait_for(3).await;
    let shed = records.iter().find(|f| f.exchange.limits.refusal == Some(Refusal::Queue)).unwrap();
    assert_eq!((shed.status, shed.outcome), (503, Outcome::Rejected));
    let left = records.iter().find(|f| f.outcome == Outcome::ClientCancelled).expect("the queued request is recorded");
    assert_eq!(left.status, 499);
}

#[tokio::test]
async fn joined_requests_share_the_leaders_slot() {
    let h = Harness::start_with(|c| c.cache.exact = true).await;
    let (name, value) = script(r#"{"ttfb_ms":300}"#);
    let leader = {
        let request = h.post("/api/chat", ask("same")).header(name, value.clone());
        tokio::spawn(async move { request.send().await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The slot is taken, yet the identical request doesn't queue: it joins the leader's call.
    let joined = h.post("/api/chat", ask("same")).header(name, value).send().await.unwrap();
    assert_eq!(joined.headers()["x-jetway-cache"], "joined");
    assert!(!joined.headers().contains_key("x-jetway-queue-wait"));
    let leader = leader.await.unwrap();
    assert_eq!(leader.bytes().await.unwrap(), joined.bytes().await.unwrap());
    assert_eq!(h.fake.events.requests().len(), 1);
}

#[tokio::test]
async fn the_output_cap_is_added_only_where_the_request_sets_none() {
    let h = Harness::start_with(|c| {
        c.apps.insert("pp".into(), app(|a| a.max_output_tokens = Some(64)));
    })
    .await;
    h.post("/a/pp/api/chat", ask("no cap")).send().await.unwrap().bytes().await.unwrap();
    let mut own = ask("own cap");
    own["options"] = json!({"num_predict": 8});
    h.post("/a/pp/api/chat", own).send().await.unwrap().bytes().await.unwrap();
    h.post("/a/pp/v1/chat/completions", json!({"model": "m", "messages": []})).send().await.unwrap();

    let bodies: Vec<Value> =
        h.fake.events.requests().iter().map(|(_, _, _, _, b)| serde_json::from_slice(b).unwrap()).collect();
    assert_eq!(bodies[0]["options"]["num_predict"], 64);
    assert_eq!(bodies[1]["options"]["num_predict"], 8);
    assert_eq!(bodies[2]["max_tokens"], 64);
    let records = h.records.wait_for(3).await;
    let caps: Vec<Option<u32>> = records.iter().map(|f| f.exchange.limits.output_cap).collect();
    assert_eq!(caps, [Some(64), None, Some(64)]);
}

#[tokio::test]
async fn with_the_upstream_down_requests_fail_fast_and_recover_when_it_returns() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fake = fake_ollama::spawn(listener, Script::default()).unwrap();
    let stack = Stack::start_with(move |c| {
        c.upstream.url = format!("http://{addr}");
        c.upstream.connect_timeout_ms = 300;
        c.health.probe_seconds = 1;
        c.health.failures_to_open = 2;
    })
    .await;
    assert_eq!(stack.post("/api/chat", chat(false)).send().await.unwrap().status(), 200);
    let health = Arc::clone(stack.running().gateway.health());
    let wait_until = |open: bool| {
        let health = Arc::clone(&health);
        async move {
            for _ in 0..100 {
                if health.is_open() == open {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("the circuit never became {}", if open { "open" } else { "closed" });
        }
    };

    drop(fake);
    wait_until(true).await;
    let started = Instant::now();
    let refused = stack.post("/api/chat", chat(false)).send().await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(200), "{:?}", started.elapsed());
    assert_eq!(refused.status(), 503);
    assert_eq!(retry_after(&refused), 1, "the probe interval");
    let body: Value = refused.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().starts_with("ollama is down"), "{body}");
    let status = stack.client.get(format!("{}/fragments/status", stack.admin_url)).send().await.unwrap();
    assert!(status.text().await.unwrap().contains("is down"));

    // Ollama comes back on the same port; the next probe closes the circuit.
    let back = fake_ollama::spawn(TcpListener::bind(addr).await.unwrap(), Script::default()).unwrap();
    wait_until(false).await;
    assert_eq!(stack.post("/api/chat", chat(false)).send().await.unwrap().status(), 200);
    assert_eq!(back.events.requests().len(), 1);
    let records = stack.records.wait_for(3).await;
    assert_eq!(records.iter().filter(|f| f.exchange.limits.refusal == Some(Refusal::CircuitOpen)).count(), 1);
}

/// Requests the fake Telegram received: path and JSON body.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A stand-in for the Telegram Bot API that keeps what it was sent.
async fn fake_telegram(status: u16) -> (String, Seen) {
    use axum::extract::{Request, State};
    let seen: Seen = Arc::default();
    let app = axum::Router::new()
        .fallback(move |State(seen): State<Seen>, request: Request| async move {
            let path = request.uri().path().to_string();
            let body = axum::body::to_bytes(request.into_body(), 1 << 20).await.unwrap();
            seen.lock().unwrap().push((path, serde_json::from_slice(&body).unwrap_or(Value::Null)));
            (axum::http::StatusCode::from_u16(status).unwrap(), r#"{"ok":false,"description":"no"}"#)
        })
        .with_state(Arc::clone(&seen));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, seen)
}

#[tokio::test]
async fn alerts_reach_telegram_and_errors_never_show_the_token() {
    let (url, seen) = fake_telegram(200).await;
    let telegram = Telegram::new(&url, "123:secret".into(), "42".into()).unwrap();
    telegram.send("[critical] ollama is down").await.unwrap();
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent[0].0, "/bot123:secret/sendMessage");
    assert_eq!(sent[0].1, json!({"chat_id": "42", "text": "Jetway [critical] ollama is down"}));

    let (url, _) = fake_telegram(401).await;
    let error = Telegram::new(&url, "123:secret".into(), "42".into()).unwrap().send("x").await.unwrap_err();
    assert!(error.starts_with("Telegram answered 401"), "{error}");
    let unreachable = Telegram::new("http://127.0.0.1:9", "123:secret".into(), "42".into()).unwrap();
    let error = unreachable.send("x").await.unwrap_err();
    assert!(!error.contains("secret"), "{error}");
}

#[tokio::test]
async fn the_alert_loop_fires_on_a_used_up_budget_and_shows_a_banner() {
    let stack = Stack::start_with(|c| {
        c.apps.insert("pp".into(), app(|a| a.requests_per_day = Some(1)));
        c.budgets.time_zone = "UTC".into();
    })
    .await;
    stack.post("/a/pp/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    stack.settle(1).await;
    let running = stack.running();
    let notices = jetway::app::evaluate_alerts(&running.gateway, &running.metrics, &running.store, &running.alerts);
    assert_eq!(notices, ["[critical] pp has used its budget: 1 of 1 requests today"]);
    let banner = stack.client.get(format!("{}/fragments/alerts", stack.admin_url)).send().await.unwrap();
    assert!(banner.text().await.unwrap().contains("pp has used its budget"));
    let page = stack.client.get(format!("{}/alerts", stack.admin_url)).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("1 of 1 requests today"));
    // Evaluating again changes nothing.
    assert!(
        jetway::app::evaluate_alerts(&running.gateway, &running.metrics, &running.store, &running.alerts).is_empty()
    );
}
