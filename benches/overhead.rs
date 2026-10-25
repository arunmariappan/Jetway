//! Phase 1.10: the latency Jetway adds, per request and per stream chunk, against the fake Ollama with no delays.
//! `cargo bench --bench overhead` (release build). Targets: p50 under 0.3 ms and p99 under 1 ms per request at
//! concurrency 1; p99 under 0.5 ms per chunk. Runs the whole of `jetway serve` (recording, store, metrics), with
//! a temporary database.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use jetway::config::Config;
use jetway::dev::fake_ollama::{self, SCRIPT_HEADER, Script};
use tokio::net::TcpListener;
use tokio::time::Instant;

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1e3)
}

const REQUESTS: usize = 2000;
const STREAM_CHUNKS: usize = 200;

#[tokio::main]
async fn main() {
    let fake = fake_ollama::spawn_local(Script::default()).await.unwrap();
    let dir = std::env::temp_dir().join(format!("jetway-bench-{}", std::process::id()));
    let mut config = Config::default();
    config.upstream.url = fake.url();
    config.store.path = dir.join("jetway.db");
    let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = jetway::app::start(config, gateway, admin, Vec::new()).await.unwrap();
    let jetway = format!("http://{}", running.gateway_addr);
    let client = reqwest::Client::builder().no_proxy().tcp_nodelay(true).build().unwrap();
    let body = r#"{"model":"qwen3.5:4b","messages":[{"role":"user","content":"hi"}],"stream":false}"#;

    // Per request: alternate direct and through Jetway so both see the same machine state.
    let mut direct = Vec::with_capacity(REQUESTS);
    let mut through = Vec::with_capacity(REQUESTS);
    for i in 0..REQUESTS + 200 {
        for (base, samples) in [(fake.url(), &mut direct), (jetway.clone(), &mut through)] {
            let started = Instant::now();
            let response = client.post(format!("{base}/api/chat")).body(body).send().await.unwrap();
            response.bytes().await.unwrap();
            if i >= 200 {
                samples.push(started.elapsed());
            }
        }
    }
    direct.sort();
    through.sort();
    println!("per request ({REQUESTS} non-streamed /api/chat, concurrency 1):");
    for (label, p) in [("p50", 0.5), ("p99", 0.99)] {
        let (d, t) = (percentile(&direct, p), percentile(&through, p));
        println!("  {label}: direct {}, through Jetway {}, added {}", ms(d), ms(t), ms(t.saturating_sub(d)));
    }

    // Per chunk: the delay from the fake sending a chunk to the client receiving it.
    let script = format!(r#"{{"gap_ms":5,"chunks":{STREAM_CHUNKS}}}"#);
    let stream_body = r#"{"model":"qwen3.5:4b","messages":[{"role":"user","content":"hi"}]}"#;
    for (label, base) in [("direct", fake.url()), ("through Jetway", jetway.clone())] {
        let mut delays = Vec::new();
        for _ in 0..3 {
            let before = fake.events.requests().len();
            let mut response = client
                .post(format!("{base}/api/chat"))
                .header(SCRIPT_HEADER, &script)
                .body(stream_body)
                .send()
                .await
                .unwrap();
            let mut marks = Vec::new();
            let mut total = 0;
            while let Some(chunk) = response.chunk().await.unwrap() {
                total += chunk.len();
                marks.push((Instant::now(), total));
            }
            let id = fake.events.requests()[before].0;
            let mut end = 0;
            for (sent_at, bytes) in fake.events.chunks_sent(id) {
                end += bytes.len();
                let (received_at, _) = marks.iter().find(|(_, len)| *len >= end).copied().unwrap();
                delays.push(received_at.duration_since(sent_at));
            }
        }
        delays.sort();
        println!(
            "per chunk {label} ({} chunks): p50 {}, p99 {}, max {}",
            delays.len(),
            ms(percentile(&delays, 0.5)),
            ms(percentile(&delays, 0.99)),
            ms(percentile(&delays, 1.0))
        );
    }
    let recorded = running.store.count().await.unwrap();
    println!(
        "recorded {recorded} exchanges, {} dropped",
        running.store.counters.dropped.load(std::sync::atomic::Ordering::Relaxed)
    );
    running.stop();
    running.wait().await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}
