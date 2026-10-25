//! What a cache entry holds and how a hit is served (phase 3.3). An entry keeps the bytes the first client received
//! plus a protocol-neutral answer. A hit in the same stream mode replays the bytes exactly; a hit in the other mode
//! is written from the answer in the request's protocol, with fresh timestamps and ids.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::protocol::Protocol;
use crate::protocol::summary::ResponseSummary;

/// The parts of an answer every protocol can express.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub model: Option<String>,
    pub content: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<Value>,
    pub done_reason: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub load_ns: Option<u64>,
    pub prompt_eval_ns: Option<u64>,
    pub eval_ns: Option<u64>,
    pub total_ns: Option<u64>,
}

impl Answer {
    pub fn from_summary(summary: &ResponseSummary) -> Self {
        Self {
            model: summary.model.clone(),
            content: summary.content.clone(),
            thinking: summary.thinking.clone(),
            tool_calls: summary.tool_call_values.clone(),
            done_reason: summary.done_reason.clone(),
            prompt_tokens: summary.prompt_tokens,
            completion_tokens: summary.completion_tokens,
            load_ns: summary.load_ns,
            prompt_eval_ns: summary.prompt_eval_ns,
            eval_ns: summary.eval_ns,
            total_ns: summary.total_ns,
        }
    }
}

/// One cache entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub app: String,
    /// The request path, which also fixes the protocol.
    pub path: String,
    /// Whether `raw` is a stream.
    pub stream: bool,
    pub content_type: String,
    /// The response as the first client received it (OpenAI streams without a usage event).
    pub raw: Bytes,
    pub answer: Answer,
    /// The exchange that produced the answer.
    pub source: String,
    pub created_ms: i64,
    pub expires_ms: i64,
}

impl Entry {
    /// What the entry costs in memory, for the cache's size bound.
    pub fn weight(&self) -> u32 {
        let bytes = self.raw.len() + self.answer.content.len() + self.answer.thinking.len() + self.path.len() + 256;
        u32::try_from(bytes).unwrap_or(u32::MAX)
    }
}

/// A served hit: content type and the body, as pieces (one piece unless paced).
pub struct Served {
    pub content_type: String,
    pub pieces: Vec<Bytes>,
}

const NDJSON: &str = "application/x-ndjson";
const SSE: &str = "text/event-stream";
const JSON: &str = "application/json; charset=utf-8";

fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

fn now_seconds() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn openai_id(prefix: &str) -> String {
    format!("{prefix}-{}", rand::random::<u32>() % 1_000_000)
}

/// OpenAI's `usage`, when the token counts are known (Jetway records them for every stream, plan D21).
fn usage(answer: &Answer) -> Option<Value> {
    if answer.prompt_tokens.is_none() && answer.completion_tokens.is_none() {
        return None;
    }
    let (p, c) = (answer.prompt_tokens.unwrap_or(0), answer.completion_tokens.unwrap_or(0));
    Some(json!({"prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c}))
}

fn with_usage(mut body: Value, answer: &Answer) -> Value {
    if let Some(usage) = usage(answer) {
        body["usage"] = usage;
    }
    body
}

/// Ollama's statistics on the final object or line.
fn ollama_stats(object: &mut serde_json::Map<String, Value>, answer: &Answer) {
    object.insert("done".into(), Value::Bool(true));
    object.insert("done_reason".into(), json!(answer.done_reason.as_deref().unwrap_or("stop")));
    for (name, value) in [
        ("total_duration", answer.total_ns),
        ("load_duration", answer.load_ns),
        ("prompt_eval_count", answer.prompt_tokens),
        ("prompt_eval_duration", answer.prompt_eval_ns),
        ("eval_count", answer.completion_tokens),
        ("eval_duration", answer.eval_ns),
    ] {
        if let Some(value) = value {
            object.insert(name.into(), json!(value));
        }
    }
}

fn line(value: &Value) -> Bytes {
    let mut text = value.to_string();
    text.push('\n');
    Bytes::from(text)
}

fn event(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

/// Splits recorded bytes into whole lines (NDJSON) or events (SSE), keeping their terminators, for paced replay.
pub fn split_items(raw: &Bytes, sse: bool) -> Vec<Bytes> {
    let separator: &[u8] = if sse { b"\n\n" } else { b"\n" };
    let mut items = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + separator.len() <= raw.len() {
        if &raw[i..i + separator.len()] == separator {
            items.push(raw.slice(start..i + separator.len()));
            i += separator.len();
            start = i;
        } else {
            i += 1;
        }
    }
    if start < raw.len() {
        items.push(raw.slice(start..));
    }
    items
}

/// Serves `entry` for a request in `protocol` at `path` that wants `stream` (and, for OpenAI streams, usage).
pub fn serve(entry: &Entry, protocol: Protocol, stream: bool, include_usage: bool) -> Served {
    if stream == entry.stream {
        let mut raw = entry.raw.clone();
        if protocol == Protocol::OpenAi && stream && include_usage {
            raw = with_usage_event(&raw, &entry.answer);
        }
        return Served { content_type: entry.content_type.clone(), pieces: vec![raw] };
    }
    let model = entry.answer.model.clone().unwrap_or_default();
    let a = &entry.answer;
    match (protocol, entry.path.as_str(), stream) {
        (Protocol::Ollama, "/api/chat", _) => {
            let mut message = json!({"role": "assistant", "content": a.content});
            if !a.thinking.is_empty() {
                message["thinking"] = json!(a.thinking);
            }
            if !a.tool_calls.is_empty() {
                message["tool_calls"] = json!(a.tool_calls);
            }
            ollama_reply(
                &model,
                stream,
                |object| {
                    object.insert("message".into(), message.clone());
                },
                |object| {
                    object.insert("message".into(), json!({"role": "assistant", "content": ""}));
                },
                a,
            )
        }
        (Protocol::Ollama, _, _) => {
            let thinking = a.thinking.clone();
            ollama_reply(
                &model,
                stream,
                |object| {
                    object.insert("response".into(), json!(a.content));
                    if !thinking.is_empty() {
                        object.insert("thinking".into(), json!(thinking));
                    }
                },
                |object| {
                    object.insert("response".into(), json!(""));
                },
                a,
            )
        }
        (Protocol::OpenAi, "/v1/completions", false) => {
            let body = json!({
                "id": openai_id("cmpl"), "object": "text_completion", "created": now_seconds(), "model": model,
                "system_fingerprint": "fp_ollama",
                "choices": [{"text": a.content, "index": 0, "finish_reason": a.done_reason.as_deref().unwrap_or("stop")}],
            });
            Served { content_type: JSON.into(), pieces: vec![Bytes::from(with_usage(body, a).to_string())] }
        }
        (Protocol::OpenAi, "/v1/completions", true) => {
            let (id, created) = (openai_id("cmpl"), now_seconds());
            let chunk = |text: &str, reason: Option<&str>| {
                event(&json!({
                    "id": id, "object": "text_completion", "created": created, "model": model,
                    "system_fingerprint": "fp_ollama",
                    "choices": [{"text": text, "index": 0, "finish_reason": reason}],
                }))
            };
            let mut body = Vec::new();
            body.extend_from_slice(&chunk(&a.content, None));
            body.extend_from_slice(&chunk("", Some(a.done_reason.as_deref().unwrap_or("stop"))));
            body.extend_from_slice(b"data: [DONE]\n\n");
            Served { content_type: SSE.into(), pieces: vec![Bytes::from(body)] }
        }
        (Protocol::OpenAi, _, false) => {
            let mut message = json!({"role": "assistant", "content": a.content});
            if !a.thinking.is_empty() {
                message["reasoning"] = json!(a.thinking);
            }
            if !a.tool_calls.is_empty() {
                message["tool_calls"] = json!(a.tool_calls);
            }
            let body = json!({
                "id": openai_id("chatcmpl"), "object": "chat.completion", "created": now_seconds(), "model": model,
                "system_fingerprint": "fp_ollama",
                "choices": [{"index": 0, "message": message, "finish_reason": a.done_reason.as_deref().unwrap_or("stop")}],
            });
            Served { content_type: JSON.into(), pieces: vec![Bytes::from(with_usage(body, a).to_string())] }
        }
        (Protocol::OpenAi, _, true) => {
            let (id, created) = (openai_id("chatcmpl"), now_seconds());
            let chunk = |delta: Value, reason: Option<&str>| {
                json!({
                    "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
                    "system_fingerprint": "fp_ollama",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
                })
            };
            let mut delta = json!({"role": "assistant", "content": a.content});
            if !a.thinking.is_empty() {
                delta["reasoning"] = json!(a.thinking);
            }
            if !a.tool_calls.is_empty() {
                let calls: Vec<Value> = a
                    .tool_calls
                    .iter()
                    .enumerate()
                    .map(|(i, call)| {
                        let mut call = call.clone();
                        call["index"] = json!(i);
                        call
                    })
                    .collect();
                delta["tool_calls"] = json!(calls);
            }
            let mut body = Vec::new();
            body.extend_from_slice(&event(&chunk(delta, None)));
            body.extend_from_slice(&event(&chunk(json!({}), Some(a.done_reason.as_deref().unwrap_or("stop")))));
            if include_usage && let Some(usage) = usage(a) {
                let mut usage_event = chunk(json!({}), None);
                usage_event["choices"] = json!([]);
                usage_event["usage"] = usage;
                body.extend_from_slice(&event(&usage_event));
            }
            body.extend_from_slice(b"data: [DONE]\n\n");
            Served { content_type: SSE.into(), pieces: vec![Bytes::from(body)] }
        }
    }
}

/// An Ollama reply from an answer: one content line and a final line (stream), or one object.
fn ollama_reply(
    model: &str,
    stream: bool,
    content: impl Fn(&mut serde_json::Map<String, Value>),
    empty: impl Fn(&mut serde_json::Map<String, Value>),
    answer: &Answer,
) -> Served {
    let base = || {
        let mut object = serde_json::Map::new();
        object.insert("model".into(), json!(model));
        object.insert("created_at".into(), json!(now_rfc3339()));
        object
    };
    if stream {
        let mut first = base();
        content(&mut first);
        first.insert("done".into(), Value::Bool(false));
        let mut last = base();
        empty(&mut last);
        ollama_stats(&mut last, answer);
        let mut body = line(&Value::Object(first)).to_vec();
        body.extend_from_slice(&line(&Value::Object(last)));
        Served { content_type: NDJSON.into(), pieces: vec![Bytes::from(body)] }
    } else {
        let mut object = base();
        content(&mut object);
        ollama_stats(&mut object, answer);
        Served { content_type: JSON.into(), pieces: vec![Bytes::from(Value::Object(object).to_string())] }
    }
}

/// Adds a usage event before `[DONE]` in an OpenAI stream stored without one (if the counts are known).
fn with_usage_event(raw: &Bytes, answer: &Answer) -> Bytes {
    let Some(usage) = usage(answer) else { return raw.clone() };
    let usage_event = event(&json!({
        "id": openai_id("chatcmpl"), "object": "chat.completion.chunk", "created": now_seconds(),
        "model": answer.model.clone().unwrap_or_default(), "system_fingerprint": "fp_ollama",
        "choices": [], "usage": usage,
    }));
    let done = b"data: [DONE]\n\n";
    let at = raw.windows(done.len()).rposition(|w| w == done).unwrap_or(raw.len());
    let mut body = Vec::with_capacity(raw.len() + usage_event.len());
    body.extend_from_slice(&raw[..at]);
    body.extend_from_slice(&usage_event);
    body.extend_from_slice(&raw[at..]);
    Bytes::from(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::decode::{NdjsonDecoder, SseDecoder};

    fn fixture(name: &str) -> Bytes {
        Bytes::from(std::fs::read(format!("{}/tests/fixtures/ollama/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap())
    }

    fn summarize(protocol: Protocol, body: &[u8], content_type: &str) -> ResponseSummary {
        let mut items = Vec::new();
        if content_type.starts_with(NDJSON) {
            let mut d = NdjsonDecoder::default();
            d.push(body, &mut items);
            items.extend(d.finish());
        } else if content_type.starts_with(SSE) {
            let mut d = SseDecoder::default();
            d.push(body, &mut items);
            items.extend(d.finish());
        } else {
            items.push(body.to_vec());
        }
        let mut summary = ResponseSummary::default();
        for item in items {
            summary.feed(protocol, &item);
        }
        summary
    }

    fn entry(protocol: Protocol, path: &str, name: &str, stream: bool, content_type: &str) -> Entry {
        let raw = fixture(name);
        let summary = summarize(protocol, &raw, content_type);
        Entry {
            app: "pp".into(),
            path: path.into(),
            stream,
            content_type: content_type.into(),
            raw,
            answer: Answer::from_summary(&summary),
            source: "x".into(),
            created_ms: 0,
            expires_ms: i64::MAX,
        }
    }

    /// Every stored mode served in every requested mode says the same thing.
    #[test]
    fn served_answers_match_the_recorded_one() {
        let cases = [
            (Protocol::Ollama, "/api/chat", "chat-stream.ndjson", true, NDJSON),
            (Protocol::Ollama, "/api/chat", "chat.json", false, JSON),
            (Protocol::Ollama, "/api/chat", "chat-think-stream.ndjson", true, NDJSON),
            (Protocol::Ollama, "/api/chat", "chat-tools.json", false, JSON),
            (Protocol::Ollama, "/api/generate", "generate-stream.ndjson", true, NDJSON),
            (Protocol::Ollama, "/api/generate", "generate.json", false, JSON),
            (Protocol::OpenAi, "/v1/chat/completions", "openai-chat-stream.sse", true, SSE),
            (Protocol::OpenAi, "/v1/chat/completions", "openai-chat.json", false, JSON),
            (Protocol::OpenAi, "/v1/chat/completions", "openai-chat-think-stream.sse", true, SSE),
        ];
        for (protocol, path, name, stream, content_type) in cases {
            let entry = entry(protocol, path, name, stream, content_type);
            for wanted in [true, false] {
                let served = serve(&entry, protocol, wanted, false);
                let body: Vec<u8> = served.pieces.concat();
                assert_eq!(served.content_type.starts_with(JSON), !wanted, "{name} as stream={wanted}");
                let again = summarize(protocol, &body, &served.content_type);
                assert_eq!(again.content, entry.answer.content, "{name} as stream={wanted}");
                assert_eq!(again.thinking, entry.answer.thinking, "{name} as stream={wanted}");
                assert_eq!(again.tool_call_values.len(), entry.answer.tool_calls.len(), "{name} as stream={wanted}");
                assert!(again.done, "{name} as stream={wanted}");
                if protocol == Protocol::Ollama || !wanted {
                    assert_eq!(again.completion_tokens, entry.answer.completion_tokens, "{name} as stream={wanted}");
                }
                if wanted == stream {
                    assert_eq!(body, entry.raw, "same mode replays the bytes exactly");
                }
            }
        }
    }

    #[test]
    fn openai_streams_get_a_usage_event_when_asked() {
        let entry = entry(Protocol::OpenAi, "/v1/chat/completions", "openai-chat-stream.sse", true, SSE);
        let entry = Entry {
            answer: Answer { prompt_tokens: Some(18), completion_tokens: Some(10), ..entry.answer.clone() },
            ..entry
        };
        for stored_stream in [true, false] {
            let entry = Entry { stream: stored_stream, ..entry.clone() };
            let body: Vec<u8> = serve(&entry, Protocol::OpenAi, true, true).pieces.concat();
            let summary = summarize(Protocol::OpenAi, &body, SSE);
            assert_eq!((summary.prompt_tokens, summary.completion_tokens), (Some(18), Some(10)));
            assert!(body.ends_with(b"data: [DONE]\n\n"));
        }
    }

    #[test]
    fn items_split_for_paced_replay() {
        let raw = fixture("chat-stream.ndjson");
        let items = split_items(&raw, false);
        assert_eq!(items.len(), 10);
        assert_eq!(items.concat(), raw.to_vec());
        let sse = fixture("openai-chat-stream.sse");
        assert_eq!(split_items(&sse, true).concat(), sse.to_vec());
    }
}
