//! `jetway load` (phase 4.5, extended in phase 10): open-loop traffic at a fixed rate, for demos and dashboard work
//! against the fake Ollama. Requests mix apps, both APIs, streamed and whole answers, repeated questions (cache hits)
//! and, with `--fake`, scripted timings and a few failures.

use std::time::Duration;

use rand::seq::IndexedRandom;
use rand::{RngExt, SeedableRng};
use serde_json::json;
use tokio::time::Instant;

use super::fake_ollama::SCRIPT_HEADER;

#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub target: String,
    pub rate: f64,
    pub duration: Duration,
    pub apps: Vec<String>,
    /// Share of requests that repeat an earlier question.
    pub repeat: f64,
    /// Add fake-Ollama scripts (timings, a few errors and drops).
    pub fake: bool,
}

#[derive(Debug, Default)]
pub struct LoadReport {
    pub sent: usize,
    pub statuses: std::collections::BTreeMap<u16, usize>,
    pub failed: usize,
    pub totals: Vec<Duration>,
}

const TOPICS: [&str; 24] = [
    "transformer attention",
    "retrieval augmented generation",
    "mixture of experts",
    "speculative decoding",
    "KV cache paging",
    "LoRA fine-tuning",
    "prompt injection defences",
    "tool-calling agents",
    "evaluation with LLM judges",
    "quantisation to 4 bits",
    "continuous batching",
    "sparse attention",
    "state space models",
    "reward models",
    "direct preference optimisation",
    "chain of thought",
    "embedding models",
    "vector search",
    "semantic caching",
    "guardrails",
    "agent memory",
    "model distillation",
    "long context",
    "code generation",
];

fn question(rng: &mut impl rand::Rng) -> String {
    let topic = TOPICS.choose(rng).copied().unwrap_or("transformers");
    let form = ["What is", "Explain", "Summarise recent work on", "Give two examples of", "Compare approaches to"]
        .choose(rng)
        .copied()
        .unwrap_or("Explain");
    format!("{form} {topic} (#{})", rng.random_range(0..1000))
}

/// One request: path and body.
fn request(rng: &mut impl rand::Rng, text: &str) -> (&'static str, serde_json::Value) {
    match rng.random_range(0..4) {
        0 => (
            "/api/chat",
            json!({"model": "qwen3.5:4b", "messages": [{"role": "user", "content": text}], "think": false}),
        ),
        1 => (
            "/api/chat",
            json!({"model": "qwen3.5:4b", "messages": [{"role": "user", "content": text}], "think": false, "stream": false}),
        ),
        2 => ("/api/generate", json!({"model": "qwen3.5:4b", "prompt": text, "think": false})),
        _ => (
            "/v1/chat/completions",
            json!({"model": "qwen3.5:4b", "messages": [{"role": "user", "content": text}], "stream": true}),
        ),
    }
}

fn script(rng: &mut impl rand::Rng) -> String {
    let roll: f64 = rng.random();
    if roll < 0.02 {
        return r#"{"status":500}"#.into();
    }
    if roll < 0.03 {
        return r#"{"drop_after":3}"#.into();
    }
    format!(
        r#"{{"ttfb_ms":{},"gap_ms":{},"chunks":{}}}"#,
        rng.random_range(40..600),
        rng.random_range(5..30),
        rng.random_range(5..40)
    )
}

pub async fn run(options: LoadOptions) -> LoadReport {
    let client = match reqwest::Client::builder().no_proxy().tcp_nodelay(true).build() {
        Ok(client) => client,
        Err(_) => return LoadReport::default(),
    };
    let interval = Duration::from_secs_f64(1.0 / options.rate.max(0.01));
    let started = Instant::now();
    let mut asked: Vec<String> = Vec::new();
    let mut tasks = Vec::new();
    let mut ticker = tokio::time::interval(interval);
    while started.elapsed() < options.duration {
        ticker.tick().await;
        let mut rng = rand::rng();
        let text = if !asked.is_empty() && rng.random::<f64>() < options.repeat {
            asked.choose(&mut rng).cloned().unwrap_or_default()
        } else {
            let q = question(&mut rng);
            asked.push(q.clone());
            q
        };
        let app = options.apps.choose(&mut rng).cloned().unwrap_or_else(|| "default".into());
        // The same question always goes the same way, so repeats are exact repeats.
        let mut seeded = rand::rngs::StdRng::seed_from_u64(
            text.bytes().fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b))),
        );
        let (path, body) = request(&mut seeded, &text);
        let mut builder = client
            .post(format!("{}/a/{app}{path}", options.target.trim_end_matches('/')))
            .header("content-type", "application/json")
            .body(body.to_string());
        if options.fake {
            builder = builder.header(SCRIPT_HEADER, script(&mut rng));
        }
        tasks.push(tokio::spawn(async move {
            let t = Instant::now();
            match builder.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let ok = response.bytes().await.is_ok();
                    (Some(status), ok, t.elapsed())
                }
                Err(_) => (None, false, t.elapsed()),
            }
        }));
    }
    let mut report = LoadReport::default();
    for task in tasks {
        report.sent += 1;
        match task.await {
            Ok((Some(status), ok, took)) => {
                *report.statuses.entry(status).or_default() += 1;
                report.failed += usize::from(!ok);
                report.totals.push(took);
            }
            _ => report.failed += 1,
        }
    }
    report.totals.sort();
    report
}
