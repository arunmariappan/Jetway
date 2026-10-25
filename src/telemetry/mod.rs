//! OpenTelemetry spans (plan D16, phase 2.5). Span ids are chosen when a request arrives, so the upstream call can
//! carry Jetway's client span as its parent, but the spans themselves are built only when the exchange ends, from
//! its recorded timings: nothing is held open during a stream. Spans are exported in batches over OTLP/HTTP with
//! protobuf, by a task using Jetway's own HTTP client.

use std::time::Duration;

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::SpanKind;
use opentelemetry_proto::tonic::trace::v1::status::StatusCode;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::proxy::record::{Finished, Outcome, Sink};
use crate::server::routes::PathClass;

const BATCH: usize = 512;
const FLUSH_EVERY: Duration = Duration::from_secs(2);
const QUEUE: usize = 8192;

/// The ids of one exchange's spans, fixed when the request arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanIds {
    pub trace_id: [u8; 16],
    /// The caller's span, from an incoming `traceparent`.
    pub parent: Option<[u8; 8]>,
    pub server: [u8; 8],
    pub client: [u8; 8],
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn nonzero<const N: usize>() -> [u8; N] {
    loop {
        let bytes: [u8; N] = rand::random();
        if bytes.iter().any(|&b| b != 0) {
            return bytes;
        }
    }
}

impl SpanIds {
    /// Continues the caller's trace when `traceparent` is valid, otherwise starts a new one.
    pub fn new(traceparent: Option<&str>) -> Self {
        let incoming = traceparent.and_then(|value| {
            let mut parts = value.trim().split('-');
            let (_version, trace, span) = (parts.next()?, parts.next()?, parts.next()?);
            let trace = unhex::<16>(trace).filter(|t| t.iter().any(|&b| b != 0))?;
            let span = unhex::<8>(span).filter(|s| s.iter().any(|&b| b != 0))?;
            Some((trace, span))
        });
        let (trace_id, parent) = match incoming {
            Some((trace, span)) => (trace, Some(span)),
            None => (nonzero(), None),
        };
        Self { trace_id, parent, server: nonzero(), client: nonzero() }
    }

    /// The `traceparent` sent upstream: Jetway's client span is the parent of whatever the upstream does.
    pub fn upstream_traceparent(&self) -> String {
        format!("00-{}-{}-01", hex(&self.trace_id), hex(&self.client))
    }
}

fn kv(key: &str, value: any_value::Value) -> KeyValue {
    KeyValue { key: key.to_string(), value: Some(AnyValue { value: Some(value) }), ..KeyValue::default() }
}

fn string(key: &str, value: impl Into<String>) -> KeyValue {
    kv(key, any_value::Value::StringValue(value.into()))
}

fn int(key: &str, value: i64) -> KeyValue {
    kv(key, any_value::Value::IntValue(value))
}

fn double(key: &str, value: f64) -> KeyValue {
    kv(key, any_value::Value::DoubleValue(value))
}

fn epoch_ns(f: &Finished, offset: Option<Duration>) -> u64 {
    let start = u64::try_from(f.exchange.started_at.as_nanosecond()).unwrap_or(0);
    start + offset.map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(0))
}

/// The GenAI operation name for an endpoint (OpenTelemetry's GenAI semantic conventions).
fn operation(path: &str) -> &'static str {
    let path = path.split('?').next().unwrap_or(path);
    match path {
        "/api/chat" | "/v1/chat/completions" => "chat",
        "/api/generate" | "/v1/completions" => "text_completion",
        _ => "embeddings",
    }
}

/// The server span and, when Jetway reached the upstream, the client span of one exchange.
pub fn spans_for(f: &Finished, ids: &SpanIds, upstream: &str) -> Vec<Span> {
    let failed = f.outcome != Outcome::Ok || f.status >= 500;
    let status = || Status {
        code: if failed { StatusCode::Error as i32 } else { StatusCode::Unset as i32 },
        message: f.error.clone().unwrap_or_default(),
    };
    let path = f.exchange.path.split('?').next().unwrap_or(&f.exchange.path).to_string();
    let end = epoch_ns(f, f.timings.done);

    let server = Span {
        trace_id: ids.trace_id.to_vec(),
        span_id: ids.server.to_vec(),
        parent_span_id: ids.parent.map(|p| p.to_vec()).unwrap_or_default(),
        name: format!("{} {path}", f.exchange.method),
        kind: SpanKind::Server as i32,
        start_time_unix_nano: epoch_ns(f, None),
        end_time_unix_nano: end,
        attributes: vec![
            string("http.request.method", f.exchange.method.as_str()),
            string("url.path", path.clone()),
            int("http.response.status_code", i64::from(f.status)),
            string("jetway.app", f.exchange.app.clone()),
            string("jetway.exchange_id", f.exchange.id.to_string()),
            string("jetway.outcome", f.outcome.as_str()),
        ],
        status: Some(status()),
        ..Span::default()
    };
    let Some(sent) = f.timings.upstream_sent else { return vec![server] };

    let mut attributes = vec![string("server.address", upstream.to_string())];
    let genai = matches!(f.exchange.class, PathClass::Generation | PathClass::Embedding);
    let name = if genai {
        let op = operation(&path);
        let request = f.exchange.summary.as_ref();
        let model = request.and_then(|r| r.model.clone()).unwrap_or_default();
        attributes.push(string("gen_ai.operation.name", op));
        attributes.push(string("gen_ai.provider.name", "ollama"));
        attributes.push(string("gen_ai.request.model", model.clone()));
        if let Some(r) = request {
            if let Some(t) = r.temperature {
                attributes.push(double("gen_ai.request.temperature", t));
            }
            if let Some(p) = r.top_p {
                attributes.push(double("gen_ai.request.top_p", p));
            }
            if let Some(m) = r.max_tokens {
                attributes.push(int("gen_ai.request.max_tokens", m));
            }
            if let Some(seed) = r.seed {
                attributes.push(int("gen_ai.request.seed", seed));
            }
        }
        if let Some(m) = &f.summary.model {
            attributes.push(string("gen_ai.response.model", m.clone()));
        }
        if let Some(n) = f.summary.prompt_tokens {
            attributes.push(int("gen_ai.usage.input_tokens", i64::try_from(n).unwrap_or(i64::MAX)));
        }
        if let Some(n) = f.summary.completion_tokens {
            attributes.push(int("gen_ai.usage.output_tokens", i64::try_from(n).unwrap_or(i64::MAX)));
        }
        if let Some(reason) = &f.summary.done_reason {
            let reasons =
                ArrayValue { values: vec![AnyValue { value: Some(any_value::Value::StringValue(reason.clone())) }] };
            attributes.push(kv("gen_ai.response.finish_reasons", any_value::Value::ArrayValue(reasons)));
        }
        format!("{op} {model}")
    } else {
        format!("{} {path}", f.exchange.method)
    };
    let client = Span {
        trace_id: ids.trace_id.to_vec(),
        span_id: ids.client.to_vec(),
        parent_span_id: ids.server.to_vec(),
        name,
        kind: SpanKind::Client as i32,
        start_time_unix_nano: epoch_ns(f, Some(sent)),
        end_time_unix_nano: end,
        attributes,
        status: Some(status()),
        ..Span::default()
    };
    vec![server, client]
}

/// The exporter's handle: spans go into a bounded queue that a background task sends in batches.
pub struct Telemetry {
    tx: mpsc::Sender<Span>,
    upstream: String,
}

impl Telemetry {
    /// Starts the exporter for `endpoint` (an OTLP/HTTP base URL; `/v1/traces` is appended).
    pub fn start(endpoint: &str, service_name: &str, upstream: &str) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        let url = format!("{}/v1/traces", endpoint.trim_end_matches('/'));
        tokio::spawn(export(rx, url, service_name.to_string()));
        Self { tx, upstream: upstream.to_string() }
    }
}

impl Sink for Telemetry {
    fn finished(&self, f: Finished) {
        let Some(ids) = f.exchange.spans else { return };
        for span in spans_for(&f, &ids, &self.upstream) {
            // A full queue drops spans rather than slowing anything down.
            let _ = self.tx.try_send(span);
        }
    }
}

/// The OTLP request body for a batch of spans.
pub fn encode(service_name: &str, spans: Vec<Span>) -> Vec<u8> {
    let request = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource { attributes: vec![string("service.name", service_name)], ..Resource::default() }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "jetway".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    ..InstrumentationScope::default()
                }),
                spans,
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    };
    request.encode_to_vec()
}

async fn export(mut rx: mpsc::Receiver<Span>, url: String, service_name: String) {
    let client = match reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build() {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(%error, "couldn't build the OTLP client; spans are off");
            return;
        }
    };
    let mut last_warning: Option<Instant> = None;
    let mut batch = Vec::with_capacity(BATCH);
    loop {
        let deadline = tokio::time::sleep(FLUSH_EVERY);
        tokio::pin!(deadline);
        let closed = loop {
            tokio::select! {
                span = rx.recv() => match span {
                    Some(span) => {
                        batch.push(span);
                        if batch.len() >= BATCH {
                            break false;
                        }
                    }
                    None => break true,
                },
                () = &mut deadline => break false,
            }
        };
        if !batch.is_empty() {
            let body = encode(&service_name, std::mem::take(&mut batch));
            let sent = client.post(&url).header("content-type", "application/x-protobuf").body(body).send().await;
            let problem = match sent {
                Ok(response) if response.status().is_success() => None,
                Ok(response) => Some(format!("status {}", response.status())),
                Err(error) => Some(error.to_string()),
            };
            if let Some(problem) = problem
                && last_warning.is_none_or(|at| at.elapsed() > Duration::from_secs(60))
            {
                tracing::warn!(%url, %problem, "couldn't export spans (logged once a minute)");
                last_warning = Some(Instant::now());
            }
        }
        if closed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continues_a_valid_trace_and_starts_one_otherwise() {
        let ids = SpanIds::new(Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"));
        assert_eq!(hex(&ids.trace_id), "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(ids.parent.map(|p| hex(&p)).as_deref(), Some("b7ad6b7169203331"));
        let upstream = ids.upstream_traceparent();
        assert!(upstream.starts_with("00-0af7651916cd43dd8448eb211c80319c-"));
        assert_eq!(&upstream[36..52], hex(&ids.client));

        for bad in [None, Some("00-00000000000000000000000000000000-b7ad6b7169203331-01"), Some("nonsense")] {
            let ids = SpanIds::new(bad);
            assert!(ids.parent.is_none());
            assert!(ids.trace_id.iter().any(|&b| b != 0));
        }
    }
}
