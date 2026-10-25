//! Phase 4: the dashboard, its live feed, assets and the admin JSON API.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{Stack, chat};
use serde_json::{Value, json};

async fn stack_with_traffic() -> (Stack, String) {
    let stack = Stack::start_with(|c| c.cache.exact = true).await;
    stack.post("/a/pp/api/chat", chat(true)).send().await.unwrap().bytes().await.unwrap();
    stack.post("/a/pp/api/chat", chat(true)).send().await.unwrap().bytes().await.unwrap();
    stack
        .post("/a/ops/v1/chat/completions", json!({"model": "qwen3.5:4b", "messages": []}))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    stack.post("/api/nope", json!({})).send().await.unwrap().bytes().await.unwrap();
    let records = stack.settle(4).await;
    let id = records[0].exchange.id.to_string();
    (stack, id)
}

async fn get(stack: &Stack, path: &str) -> reqwest::Response {
    stack.client.get(format!("{}{path}", stack.admin_url)).send().await.unwrap()
}

#[tokio::test]
async fn every_page_renders_with_its_content() {
    let (stack, id) = stack_with_traffic().await;
    let cases: [(&str, &[&str]); 11] = [
        (
            "/",
            &[
                "Request latency",
                "Time to first token",
                "Requests by outcome",
                "Cache results",
                "<svg",
                "Cache hit rate",
            ],
        ),
        ("/?window=24h&app=pp", &["App pp", "per 15 minutes", "<option value=\"pp\" selected>"]),
        ("/?window=15m", &["aria-current=\"true\">15m"]),
        ("/fragments/overview?window=1h", &["class=\"tiles\"", "<details class=\"table-view\">"]),
        ("/fragments/status", &["ollama", "in progress"]),
        ("/fragments/inflight", &["Nothing in progress."]),
        ("/live", &["sse-connect=\"/live/events\"", "/v1/chat/completions", "hit"]),
        (&format!("/exchanges/{id}"), &["POST /api/chat", "Waiting for the upstream", "Request body", "18 / 10"]),
        ("/apps", &["pp", "ops", "default"]),
        ("/apps?window=15m", &["<a href=\"/?app=pp&window=15m\">pp</a>"]),
        ("/cache", &["Stored entries by app", "Most hit entries", "Hello there, friend!"]),
    ];
    for (path, expected) in cases {
        let response = get(&stack, path).await;
        assert_eq!(response.status(), 200, "{path}");
        let text = response.text().await.unwrap();
        for needle in expected {
            assert!(text.contains(needle), "{path} should contain {needle:?}");
        }
    }
    assert_eq!(get(&stack, "/exchanges/nope").await.status(), 404);
}

#[tokio::test]
async fn the_api_returns_the_same_data() {
    let (stack, id) = stack_with_traffic().await;
    let overview: Value = get(&stack, "/api/overview?window=15m").await.json().await.unwrap();
    assert_eq!(overview["tiles"]["requests"], 4);
    assert_eq!(overview["window"], "15m");
    assert_eq!(overview["rows"].as_array().unwrap().len(), 15);
    assert_eq!(overview["upstream"]["up"], true);

    let exchanges: Value = get(&stack, "/api/exchanges?app=pp").await.json().await.unwrap();
    let list = exchanges["exchanges"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let before = list[1]["id"].as_str().unwrap();
    let older: Value = get(&stack, &format!("/api/exchanges?before={before}")).await.json().await.unwrap();
    assert_eq!(older["exchanges"].as_array().unwrap().len(), 1, "paging back from the second exchange");

    let one: Value = get(&stack, &format!("/api/exchanges/{id}")).await.json().await.unwrap();
    assert_eq!(one["exchange"]["app"], "pp");
    assert!(one["bodies"]["request"].as_str().unwrap().contains("Say hello"));

    let apps: Value = get(&stack, "/api/apps").await.json().await.unwrap();
    let names: Vec<&str> = apps["apps"].as_array().unwrap().iter().map(|a| a["app"].as_str().unwrap()).collect();
    assert_eq!(names, ["default", "ops", "pp"]);

    let cache: Value = get(&stack, "/api/cache").await.json().await.unwrap();
    assert_eq!(cache["memory"]["entries"], 2);
    let cleared: Value = stack
        .client
        .post(format!("{}/api/cache/clear", stack.admin_url))
        .json(&json!({"app": "pp"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["deleted"], 1);
    let cache: Value = get(&stack, "/api/cache").await.json().await.unwrap();
    assert_eq!(cache["memory"]["entries"], 0, "memory is cleared too");
    assert_eq!(cache["stored"].as_array().unwrap().len(), 1, "only ops's entry is left");
}

#[tokio::test]
async fn clearing_needs_a_post() {
    let (stack, _) = stack_with_traffic().await;
    assert_eq!(get(&stack, "/cache/clear").await.status(), 405);
    let client = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let url = format!("{}/cache/clear", stack.admin_url);
    let plain = client
        .post(&url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("app=pp")
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), 303);
    assert_eq!(plain.headers()["location"], "/cache");
    let htmx = client
        .post(&url)
        .header("hx-request", "true")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(htmx.status(), 200);
    assert_eq!(htmx.headers()["hx-redirect"], "/cache");
}

#[tokio::test]
async fn the_live_feed_sends_each_finished_exchange() {
    let stack = Stack::start_with(|_| {}).await;
    let mut events = get(&stack, "/live/events?app=watched").await;
    assert_eq!(events.headers()["content-type"], "text/event-stream");
    stack.post("/a/other/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    stack.post("/a/watched/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    let mut received = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !received.contains("event: exchange") && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(Some(chunk))) = tokio::time::timeout(Duration::from_millis(500), events.chunk()).await {
            received.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    assert!(received.contains("event: exchange"), "{received}");
    assert!(received.contains("<td>watched</td>"));
    assert!(!received.contains("<td>other</td>"), "the app filter applies");
}

#[tokio::test]
async fn assets_are_embedded_with_types_and_etags() {
    let stack = Stack::start_with(|_| {}).await;
    for (path, kind) in [
        ("/assets/jetway.css", "text/css; charset=utf-8"),
        ("/assets/vendor/htmx.min.js", "text/javascript; charset=utf-8"),
        ("/assets/vendor/htmx-ext-sse.js", "text/javascript; charset=utf-8"),
    ] {
        let response = get(&stack, path).await;
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()["content-type"], kind);
        let etag = response.headers()["etag"].clone();
        let again =
            stack.client.get(format!("{}{path}", stack.admin_url)).header("if-none-match", etag).send().await.unwrap();
        assert_eq!(again.status(), 304, "{path}");
    }
    assert_eq!(get(&stack, "/assets/vendor/README.md").await.status(), 404, "notes aren't served");
    assert_eq!(get(&stack, "/assets/nope.js").await.status(), 404);
}

#[test]
fn templates_reference_only_embedded_assets_and_no_external_urls() {
    let assets = jetway::web::assets::paths();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("http://") && !text.contains("https://"), "{} must work offline", path.display());
        for reference in text.split("\"/assets/").skip(1) {
            let asset = reference.split('"').next().unwrap();
            assert!(assets.iter().any(|a| a == asset), "{} references a missing asset {asset}", path.display());
        }
    }
}

#[tokio::test]
async fn rollups_are_written_at_shutdown_for_the_next_start() {
    let mut stack = Stack::start_with(|_| {}).await;
    for _ in 0..3 {
        stack.post("/a/pp/api/chat", chat(false)).send().await.unwrap().bytes().await.unwrap();
    }
    stack.settle(3).await;
    let path = stack.dir.path().join("jetway.db");
    let running = stack.running.take().unwrap();
    running.stop();
    // Stopping writes the current minute's rollup too.
    running.wait().await.unwrap();
    let fake_url = stack.fake.url();
    let again = Stack::start_with(move |c| {
        c.store.path = path;
        c.upstream.url = fake_url;
    })
    .await;
    let rows = again.running().store.rollups_since(0).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.requests).sum::<i64>(), 3, "the rollup was written at shutdown");
    assert_eq!(rows[0].cacheable, 3);
}
