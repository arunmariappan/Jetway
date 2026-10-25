//! Exact token counts for OpenAI-compatible streams (plan D21). Ollama sends usage in a stream only when the client
//! asks with `stream_options.include_usage`, as an extra event with `"choices":[]` before `[DONE]` (spike S1).
//! When the client didn't ask, Jetway asks on its behalf and removes that event from what the client receives; the
//! recorder still reads it.

use bytes::{Bytes, BytesMut};
use serde_json::Value;

/// Adds `stream_options.include_usage = true` to an OpenAI request body. `None` if the body isn't a JSON object.
pub fn ask_for_usage(body: &[u8]) -> Option<Bytes> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object_mut()?;
    let options = object.entry("stream_options").or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !options.is_object() {
        *options = Value::Object(serde_json::Map::new());
    }
    options.as_object_mut()?.insert("include_usage".into(), Value::Bool(true));
    serde_json::to_vec(&value).ok().map(Bytes::from)
}

/// Removes usage-only events from an SSE stream. Complete events pass at once; an event split across chunks is held
/// until its blank line arrives, which Ollama never makes wait (it writes one whole event per flush).
#[derive(Debug, Default)]
pub struct UsageFilter {
    partial: BytesMut,
}

/// What one chunk turned into.
#[derive(Debug, Default, PartialEq)]
pub struct Filtered {
    pub sent: Bytes,
    pub hidden: Vec<Bytes>,
}

fn is_usage_only(event: &[u8]) -> bool {
    let Some(data) = event.strip_prefix(b"data:") else { return false };
    let data = data.trim_ascii();
    if !data.starts_with(b"{") {
        return false;
    }
    serde_json::from_slice::<Value>(data).is_ok_and(|v| {
        v.get("usage").is_some_and(|u| !u.is_null())
            && v.get("choices").and_then(Value::as_array).is_some_and(Vec::is_empty)
    })
}

/// The end of the first event in `bytes` (just past its blank line), tolerating `\r\n`.
fn event_end(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\n' {
            if bytes[i + 1] == b'\n' {
                return Some(i + 2);
            }
            if bytes[i + 1] == b'\r' && bytes.get(i + 2) == Some(&b'\n') {
                return Some(i + 3);
            }
        }
        i += 1;
    }
    None
}

impl UsageFilter {
    pub fn push(&mut self, chunk: Bytes) -> Filtered {
        // The common case: nothing held back, and the chunk is whole events with no usage-only one in it.
        if self.partial.is_empty() && chunk.ends_with(b"\n\n") && !contains(&chunk, b"\"usage\"") {
            return Filtered { sent: chunk, hidden: Vec::new() };
        }
        self.partial.extend_from_slice(&chunk);
        let mut sent = BytesMut::new();
        let mut hidden = Vec::new();
        while let Some(end) = event_end(&self.partial) {
            let event = self.partial.split_to(end).freeze();
            if is_usage_only(&event) {
                hidden.push(event);
            } else {
                sent.extend_from_slice(&event);
            }
        }
        Filtered { sent: sent.freeze(), hidden }
    }

    /// Whatever is left when the stream ends, passed on as it is.
    pub fn finish(&mut self) -> Bytes {
        std::mem::take(&mut self.partial).freeze()
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_USAGE: &str = include_str!("../../tests/fixtures/ollama/openai-chat-stream-usage.sse");
    const WITHOUT_USAGE: &str = include_str!("../../tests/fixtures/ollama/openai-chat-stream.sse");

    #[test]
    fn asks_for_usage_keeping_other_options() {
        let body = ask_for_usage(br#"{"stream":true,"stream_options":{"x":1}}"#).unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["stream_options"], serde_json::json!({"x": 1, "include_usage": true}));
        let body = ask_for_usage(br#"{"stream":true}"#).unwrap();
        assert!(String::from_utf8_lossy(&body).contains(r#""include_usage":true"#));
        assert!(ask_for_usage(b"[1]").is_none());
    }

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Bytes>) {
        let mut filter = UsageFilter::default();
        let mut sent = Vec::new();
        let mut hidden = Vec::new();
        for chunk in chunks {
            let out = filter.push(Bytes::copy_from_slice(chunk));
            sent.extend_from_slice(&out.sent);
            hidden.extend(out.hidden);
        }
        sent.extend_from_slice(&filter.finish());
        (sent, hidden)
    }

    #[test]
    fn the_usage_event_is_removed_and_the_rest_kept() {
        let (sent, hidden) = run(&[WITH_USAGE.as_bytes()]);
        assert_eq!(hidden.len(), 1);
        assert!(String::from_utf8_lossy(&hidden[0]).contains(r#""completion_tokens":10"#));
        // Without the usage event, the stream's content events and [DONE] are as Ollama sends them unasked.
        let content = |s: &[u8]| String::from_utf8_lossy(s).matches("\"content\"").count();
        assert_eq!(content(&sent), content(WITHOUT_USAGE.as_bytes()));
        assert!(sent.ends_with(b"data: [DONE]\n\n"));
    }

    #[test]
    fn any_split_gives_the_same_result() {
        let whole = run(&[WITH_USAGE.as_bytes()]);
        let bytes = WITH_USAGE.as_bytes();
        for cut in (1..bytes.len()).step_by(37) {
            assert_eq!(run(&[&bytes[..cut], &bytes[cut..]]), whole, "cut at {cut}");
        }
    }

    #[test]
    fn plain_chunks_pass_without_copying() {
        let mut filter = UsageFilter::default();
        let chunk = Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        let out = filter.push(chunk.clone());
        assert_eq!(out.sent.as_ptr(), chunk.as_ptr());
        assert!(out.hidden.is_empty());
    }
}
