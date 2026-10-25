//! What Jetway reads from requests and responses (phase 2.1): only the fields it needs, leniently. Unknown fields
//! are ignored here and still reach Ollama, because the raw bytes are what is forwarded.

use serde_json::Value;

use super::Protocol;

/// The fields of a request that the record, cache and router care about.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestSummary {
    pub model: Option<String>,
    /// Whether the response streams, after each API's default (Ollama streams unless told not to; OpenAI doesn't
    /// unless asked).
    pub stream: bool,
    /// `think` as sent (`true`, `false`, `"high"`, ...), or OpenAI's `reasoning_effort`.
    pub think: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<i64>,
    /// `options.num_predict`, or OpenAI's `max_tokens` / `max_completion_tokens`.
    pub max_tokens: Option<i64>,
    pub num_ctx: Option<i64>,
    pub messages: usize,
    pub has_images: bool,
    pub has_tools: bool,
    /// A JSON schema or JSON mode: Ollama's `format`, OpenAI's `response_format`.
    pub has_format: bool,
    /// OpenAI: whether the client asked for usage in a stream (`stream_options.include_usage`).
    pub include_usage: bool,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

impl RequestSummary {
    /// `None` when the body isn't a JSON object (an empty GET, for example).
    pub fn parse(protocol: Protocol, body: &[u8]) -> Option<Self> {
        let value: Value = serde_json::from_slice(body).ok()?;
        Self::from_value(protocol, &value)
    }

    /// From an already parsed body; `None` when it isn't a JSON object.
    pub fn from_value(protocol: Protocol, value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let get = |key: &str| object.get(key).filter(|v| !v.is_null());
        let options = get("options");
        let option = |key: &str| options.and_then(|o| o.get(key)).filter(|v| !v.is_null());
        let messages = get("messages").and_then(Value::as_array);
        let message_has_images = |m: &Value| match protocol {
            Protocol::Ollama => m.get("images").and_then(Value::as_array).is_some_and(|i| !i.is_empty()),
            Protocol::OpenAi => m
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|parts| parts.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("image_url"))),
        };
        let has_images = messages.is_some_and(|ms| ms.iter().any(message_has_images))
            || get("images").and_then(Value::as_array).is_some_and(|i| !i.is_empty());

        let summary = match protocol {
            Protocol::Ollama => Self {
                stream: get("stream").and_then(Value::as_bool).unwrap_or(true),
                think: get("think").and_then(text),
                temperature: option("temperature").and_then(Value::as_f64),
                top_p: option("top_p").and_then(Value::as_f64),
                seed: option("seed").and_then(Value::as_i64),
                max_tokens: option("num_predict").and_then(Value::as_i64),
                num_ctx: option("num_ctx").and_then(Value::as_i64),
                has_format: get("format").is_some(),
                ..Self::default()
            },
            Protocol::OpenAi => Self {
                stream: get("stream").and_then(Value::as_bool).unwrap_or(false),
                think: get("reasoning_effort").and_then(text),
                temperature: get("temperature").and_then(Value::as_f64),
                top_p: get("top_p").and_then(Value::as_f64),
                seed: get("seed").and_then(Value::as_i64),
                max_tokens: get("max_completion_tokens").or_else(|| get("max_tokens")).and_then(Value::as_i64),
                has_format: get("response_format").is_some(),
                include_usage: object
                    .get("stream_options")
                    .and_then(|o| o.get("include_usage"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                ..Self::default()
            },
        };
        Some(Self {
            model: get("model").and_then(Value::as_str).map(str::to_string),
            messages: messages.map_or(0, Vec::len),
            has_images,
            has_tools: get("tools").and_then(Value::as_array).is_some_and(|t| !t.is_empty()),
            ..summary
        })
    }
}

/// What the response said, accumulated over its lines or events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponseSummary {
    pub model: Option<String>,
    pub content: String,
    pub thinking: String,
    pub tool_calls: usize,
    /// The tool calls themselves, assembled from streamed pieces where needed, so a cached answer can be rewritten.
    pub tool_call_values: Vec<Value>,
    /// A final chunk (`done: true`, a `finish_reason`, `[DONE]`) or a complete unstreamed body was seen.
    pub done: bool,
    pub done_reason: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Prompt tokens served from Ollama's cache (`prompt_eval_cached_count`, OpenAI's `cached_tokens`).
    pub cached_prompt_tokens: Option<u64>,
    pub load_ns: Option<u64>,
    pub prompt_eval_ns: Option<u64>,
    pub eval_ns: Option<u64>,
    pub total_ns: Option<u64>,
    /// An error object in the body or stream.
    pub error: Option<String>,
}

/// What one line or event contained, so the recorder can time the first answer and thinking tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fed {
    pub content: bool,
    pub thinking: bool,
}

fn number(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn error_message(value: &Value) -> Option<String> {
    match value.get("error")? {
        Value::String(message) => Some(message.clone()),
        Value::Object(error) => error.get("message").and_then(Value::as_str).map(str::to_string),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

impl ResponseSummary {
    /// Adds one line (NDJSON), event (SSE) or whole body. Anything that isn't JSON is ignored, except OpenAI's
    /// `[DONE]`.
    pub fn feed(&mut self, protocol: Protocol, item: &[u8]) -> Fed {
        if item == b"[DONE]" {
            self.done = true;
            return Fed::default();
        }
        let Ok(value) = serde_json::from_slice::<Value>(item) else { return Fed::default() };
        if let Some(error) = error_message(&value) {
            self.error = Some(error);
            return Fed::default();
        }
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            self.model.get_or_insert_with(|| model.to_string());
        }
        match protocol {
            Protocol::Ollama => self.feed_ollama(&value),
            Protocol::OpenAi => self.feed_openai(&value),
        }
    }

    fn add(&mut self, content: Option<&str>, thinking: Option<&str>) -> Fed {
        let content = content.filter(|c| !c.is_empty());
        let thinking = thinking.filter(|t| !t.is_empty());
        if let Some(c) = content {
            self.content.push_str(c);
        }
        if let Some(t) = thinking {
            self.thinking.push_str(t);
        }
        Fed { content: content.is_some(), thinking: thinking.is_some() }
    }

    fn feed_ollama(&mut self, value: &Value) -> Fed {
        // /api/chat puts text in `message`; /api/generate in `response` and `thinking`.
        let message = value.get("message");
        let content = message.and_then(|m| m.get("content")).or_else(|| value.get("response"));
        let thinking = message.and_then(|m| m.get("thinking")).or_else(|| value.get("thinking"));
        let calls = message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array);
        self.tool_call_values.extend(calls.into_iter().flatten().cloned());
        let calls = calls.map_or(0, Vec::len);
        self.tool_calls += calls;
        let mut fed = self.add(content.and_then(Value::as_str), thinking.and_then(Value::as_str));
        fed.content |= calls > 0;

        let done = value.get("done").and_then(Value::as_bool);
        // An unstreamed /api/embed body has no `done`, but it is complete.
        if done == Some(true) || (done.is_none() && value.get("embeddings").is_some()) {
            self.done = true;
        }
        if let Some(reason) = value.get("done_reason").and_then(Value::as_str) {
            self.done_reason = Some(reason.to_string());
        }
        self.prompt_tokens = number(value.get("prompt_eval_count")).or(self.prompt_tokens);
        self.completion_tokens = number(value.get("eval_count")).or(self.completion_tokens);
        self.cached_prompt_tokens = number(value.get("prompt_eval_cached_count")).or(self.cached_prompt_tokens);
        self.load_ns = number(value.get("load_duration")).or(self.load_ns);
        self.prompt_eval_ns = number(value.get("prompt_eval_duration")).or(self.prompt_eval_ns);
        self.eval_ns = number(value.get("eval_duration")).or(self.eval_ns);
        self.total_ns = number(value.get("total_duration")).or(self.total_ns);
        fed
    }

    fn feed_openai(&mut self, value: &Value) -> Fed {
        let mut fed = Fed::default();
        let object = value.get("object").and_then(Value::as_str).unwrap_or_default();
        for choice in value.get("choices").and_then(Value::as_array).into_iter().flatten() {
            // Streamed chunks carry `delta`; complete chat bodies `message`; completions `text`.
            let part = choice.get("delta").or_else(|| choice.get("message"));
            let content = part.and_then(|p| p.get("content")).or_else(|| choice.get("text"));
            let thinking = part.and_then(|p| p.get("reasoning").or_else(|| p.get("reasoning_content")));
            let calls = part.and_then(|p| p.get("tool_calls")).and_then(Value::as_array).map_or(0, Vec::len);
            // Streamed tool calls arrive in pieces; count each call once, by its first piece (which has an id).
            let pieces = part.and_then(|p| p.get("tool_calls")).and_then(Value::as_array);
            let new_calls = if choice.get("delta").is_some() {
                for piece in pieces.into_iter().flatten() {
                    self.add_tool_call_piece(piece);
                }
                pieces.map_or(0, |c| c.iter().filter(|c| c.get("id").is_some()).count())
            } else {
                self.tool_call_values.extend(pieces.into_iter().flatten().cloned());
                calls
            };
            self.tool_calls += new_calls;
            let added = self.add(content.and_then(Value::as_str), thinking.and_then(Value::as_str));
            fed.content |= added.content || calls > 0;
            fed.thinking |= added.thinking;
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.done_reason = Some(reason.to_string());
                if !object.ends_with(".chunk") && object != "text_completion" {
                    self.done = true;
                }
            }
        }
        if object == "chat.completion" || object == "list" || (object == "text_completion" && !self.is_chunked(value)) {
            self.done = true;
        }
        if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
            self.prompt_tokens = number(usage.get("prompt_tokens")).or(self.prompt_tokens);
            self.completion_tokens = number(usage.get("completion_tokens")).or(self.completion_tokens);
            self.cached_prompt_tokens =
                number(usage.pointer("/prompt_tokens_details/cached_tokens")).or(self.cached_prompt_tokens);
        }
        fed
    }

    /// Merges one streamed OpenAI tool-call piece: the first piece of a call (by `index`) carries its id, type and
    /// name; later ones append to `function.arguments`.
    fn add_tool_call_piece(&mut self, piece: &Value) {
        let index = piece.get("index").and_then(Value::as_u64).unwrap_or(self.tool_call_values.len() as u64) as usize;
        while self.tool_call_values.len() <= index {
            self.tool_call_values.push(serde_json::json!({"function": {"arguments": ""}}));
        }
        let call = &mut self.tool_call_values[index];
        for key in ["id", "type"] {
            if let Some(value) = piece.get(key) {
                call[key] = value.clone();
            }
        }
        if let Some(function) = piece.get("function") {
            if let Some(name) = function.get("name") {
                call["function"]["name"] = name.clone();
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                let so_far = call["function"]["arguments"].as_str().unwrap_or_default().to_string();
                call["function"]["arguments"] = Value::String(so_far + arguments);
            }
        }
    }

    /// Ollama's streamed completions use the same `object` as unstreamed ones; only the stream has no `usage`.
    fn is_chunked(&self, value: &Value) -> bool {
        value.get("usage").is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::decode::{NdjsonDecoder, SseDecoder};

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/ollama/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn summarize(protocol: Protocol, name: &str) -> ResponseSummary {
        let body = fixture(name);
        let mut summary = ResponseSummary::default();
        let mut items = Vec::new();
        if name.ends_with(".ndjson") {
            let mut d = NdjsonDecoder::default();
            d.push(&body, &mut items);
            items.extend(d.finish());
        } else if name.ends_with(".sse") {
            let mut d = SseDecoder::default();
            d.push(&body, &mut items);
            items.extend(d.finish());
        } else {
            items.push(body);
        }
        for item in items {
            summary.feed(protocol, &item);
        }
        summary
    }

    #[test]
    fn native_chat_stream_and_body_agree() {
        let streamed = summarize(Protocol::Ollama, "chat-stream.ndjson");
        let whole = summarize(Protocol::Ollama, "chat.json");
        assert_eq!(streamed.content, whole.content);
        assert_eq!(streamed.content, "Hello there, friend! How are you?");
        assert!(streamed.done && whole.done);
        assert_eq!((streamed.prompt_tokens, streamed.completion_tokens), (Some(18), Some(10)));
        assert_eq!(streamed.done_reason.as_deref(), Some("stop"));
        assert_eq!(streamed.total_ns, Some(10_866_268_100));
        assert_eq!(whole.cached_prompt_tokens, Some(14));
    }

    #[test]
    fn thinking_tool_calls_and_generate() {
        let think = summarize(Protocol::Ollama, "chat-think-stream.ndjson");
        assert_eq!(think.content, "391");
        assert!(think.thinking.starts_with("Thinking Process"));
        assert_eq!(think.completion_tokens, Some(391));
        let tools = summarize(Protocol::Ollama, "chat-tools.json");
        assert_eq!(tools.tool_calls, 1);
        assert_eq!(tools.tool_call_values[0]["function"]["arguments"]["city"], "Pune");
        let generate = summarize(Protocol::Ollama, "generate-stream.ndjson");
        assert_eq!(generate.content, summarize(Protocol::Ollama, "generate.json").content);
        assert!(generate.done);
    }

    #[test]
    fn openai_streams_and_bodies() {
        let plain = summarize(Protocol::OpenAi, "openai-chat-stream.sse");
        assert_eq!(plain.content, "Hello there, friend! How are you?");
        assert!(plain.done);
        assert_eq!(plain.completion_tokens, None, "no usage without include_usage");
        let usage = summarize(Protocol::OpenAi, "openai-chat-stream-usage.sse");
        assert_eq!(
            (usage.prompt_tokens, usage.completion_tokens, usage.cached_prompt_tokens),
            (Some(18), Some(10), Some(14))
        );
        let whole = summarize(Protocol::OpenAi, "openai-chat.json");
        assert_eq!(whole.content, plain.content);
        assert!(whole.done);
        let think = summarize(Protocol::OpenAi, "openai-chat-think-stream.sse");
        assert_eq!(think.content, "391");
        assert!(!think.thinking.is_empty());
        let completions = summarize(Protocol::OpenAi, "openai-completions-stream.sse");
        // The recorded completion spent its 24 tokens on hidden thinking, so its text is empty.
        assert!(completions.done);
        assert_eq!(completions.done_reason.as_deref(), Some("length"));
    }

    #[test]
    fn streamed_openai_tool_calls_are_assembled() {
        let mut summary = ResponseSummary::default();
        let pieces = [
            r#"{"object":"chat.completion.chunk","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"ci"}}]}}]}"#,
            r#"{"object":"chat.completion.chunk","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ty\":\"Pune\"}"}}]}}]}"#,
            r#"{"object":"chat.completion.chunk","choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        for piece in pieces {
            summary.feed(Protocol::OpenAi, piece.as_bytes());
        }
        assert_eq!(summary.tool_calls, 1);
        let call = &summary.tool_call_values[0];
        assert_eq!((call["id"].as_str(), call["function"]["name"].as_str()), (Some("call_1"), Some("get_weather")));
        assert_eq!(call["function"]["arguments"], r#"{"city":"Pune"}"#);
    }

    #[test]
    fn errors_in_both_protocols() {
        assert_eq!(
            summarize(Protocol::Ollama, "error-unknown-model.json").error.as_deref(),
            Some("model 'no-such-model:1b' not found")
        );
        assert_eq!(
            summarize(Protocol::OpenAi, "openai-error-unknown-model.json").error.as_deref(),
            Some("model 'no-such-model:1b' not found")
        );
    }

    #[test]
    fn request_summaries() {
        let native = br#"{"model":"qwen3.5:4b","messages":[{"role":"user","content":"hi","images":["aGk="]}],
            "think":false,"format":{"type":"object"},"options":{"temperature":0.7,"seed":1,"num_predict":24,"num_ctx":4096}}"#;
        let s = RequestSummary::parse(Protocol::Ollama, native).unwrap();
        assert_eq!(s.model.as_deref(), Some("qwen3.5:4b"));
        assert!(s.stream, "Ollama streams by default");
        assert_eq!(s.think.as_deref(), Some("false"));
        assert_eq!((s.temperature, s.seed, s.max_tokens, s.num_ctx), (Some(0.7), Some(1), Some(24), Some(4096)));
        assert!(s.has_images && s.has_format && !s.has_tools);
        assert_eq!(s.messages, 1);

        let openai = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}],
            "stream":true,"stream_options":{"include_usage":true},"max_tokens":8,"reasoning_effort":"low","tools":[{}]}"#;
        let s = RequestSummary::parse(Protocol::OpenAi, openai).unwrap();
        assert!(s.stream && s.include_usage && s.has_images && s.has_tools);
        assert_eq!((s.max_tokens, s.think.as_deref()), (Some(8), Some("low")));
        assert!(
            !RequestSummary::parse(Protocol::OpenAi, br#"{"messages":[]}"#).unwrap().stream,
            "OpenAI doesn't stream by default"
        );
        assert!(RequestSummary::parse(Protocol::Ollama, b"").is_none());
    }
}
