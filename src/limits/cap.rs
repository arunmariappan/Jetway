//! The output cap (phase 9): an app's `max_output_tokens`, added to generation requests that set no cap of their own,
//! so a runaway generation can't hold the GPU for minutes (a rewrite step in phase 5 ran to 16,283 tokens).

use bytes::Bytes;
use serde_json::{Map, Value};

use crate::protocol::Protocol;

/// Adds `cap` to a generation request body that has none: Ollama's `options.num_predict`, or OpenAI's `max_tokens`.
/// A negative `num_predict` (Ollama's "no limit") counts as none. `None` when the body already has a cap or isn't
/// the JSON object these APIs expect, and is then sent as it came.
pub fn cap_output(protocol: Protocol, body: &[u8], cap: u32) -> Option<Bytes> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object_mut()?;
    match protocol {
        Protocol::Ollama => {
            let options = object.entry("options").or_insert_with(|| Value::Object(Map::new())).as_object_mut()?;
            if options.get("num_predict").and_then(Value::as_i64).is_some_and(|n| n >= 0) {
                return None;
            }
            options.insert("num_predict".into(), cap.into());
        }
        Protocol::OpenAi => {
            if ["max_tokens", "max_completion_tokens"].iter().any(|k| object.get(*k).is_some_and(|v| !v.is_null())) {
                return None;
            }
            object.insert("max_tokens".into(), cap.into());
        }
    }
    serde_json::to_vec(&value).ok().map(Bytes::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capped(protocol: Protocol, body: &str) -> Option<String> {
        cap_output(protocol, body.as_bytes(), 512).map(|b| String::from_utf8(b.to_vec()).unwrap_or_default())
    }

    #[test]
    fn adds_a_cap_only_where_there_is_none() {
        assert_eq!(
            capped(Protocol::Ollama, r#"{"model":"m","messages":[]}"#).as_deref(),
            Some(r#"{"model":"m","messages":[],"options":{"num_predict":512}}"#)
        );
        assert_eq!(
            capped(Protocol::Ollama, r#"{"model":"m","options":{"temperature":0,"num_predict":-1}}"#).as_deref(),
            Some(r#"{"model":"m","options":{"temperature":0,"num_predict":512}}"#)
        );
        assert_eq!(capped(Protocol::Ollama, r#"{"options":{"num_predict":64}}"#), None);
        assert_eq!(capped(Protocol::Ollama, r#"{"options":"odd"}"#), None);
        assert_eq!(
            capped(Protocol::OpenAi, r#"{"model":"m","stream":true}"#).as_deref(),
            Some(r#"{"model":"m","stream":true,"max_tokens":512}"#)
        );
        assert_eq!(capped(Protocol::OpenAi, r#"{"max_completion_tokens":10}"#), None);
        assert_eq!(capped(Protocol::OpenAi, "not json"), None);
    }
}
