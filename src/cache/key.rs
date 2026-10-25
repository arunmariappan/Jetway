//! The exact cache key (phase 3.1): blake3 over a canonical form of every request field that can change the answer.
//! Object keys are sorted and numbers normalised (`1` and `1.0` are equal), so formatting doesn't matter; defaults are
//! not filled in, so an omitted field and an explicit default give different keys, which is safe.

use std::fmt;

use serde_json::Value;

/// Top-level fields that don't change the answer.
const IGNORED: [&str; 5] = ["stream", "keep_alive", "stream_options", "user", "metadata"];
/// Bumped whenever the canonical form changes, so old entries can't match new keys.
const VERSION: &[u8] = b"jetway-exact-v1";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(pub [u8; 32]);

impl CacheKey {
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CacheKey({})", &self.hex()[..16])
    }
}

/// The key for a request body. `scope` is the app name, or `None` when apps share the cache.
pub fn exact_key(scope: Option<&str>, path: &str, body: &Value) -> CacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(VERSION);
    hasher.update(&[0]);
    hasher.update(scope.unwrap_or("").as_bytes());
    hasher.update(&[0]);
    hasher.update(path.as_bytes());
    hasher.update(&[0]);
    let mut canonical = Vec::with_capacity(256);
    match body {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().filter(|k| !IGNORED.contains(&k.as_str())).collect();
            keys.sort();
            canonical.push(b'{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    canonical.push(b',');
                }
                write_string(key, &mut canonical);
                canonical.push(b':');
                write_canonical(&map[key], &mut canonical);
            }
            canonical.push(b'}');
        }
        other => write_canonical(other, &mut canonical),
    }
    hasher.update(&canonical);
    CacheKey(*hasher.finalize().as_bytes())
}

/// The key for joining a request in flight: the exact key plus what shapes the bytes a client receives.
pub fn join_key(exact: &CacheKey, stream: bool, include_usage: bool) -> CacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"jetway-join-v1");
    hasher.update(&exact.0);
    hasher.update(&[u8::from(stream), u8::from(include_usage)]);
    CacheKey(*hasher.finalize().as_bytes())
}

fn write_string(text: &str, out: &mut Vec<u8>) {
    // serde_json's string escaping is deterministic.
    out.extend_from_slice(Value::String(text.to_string()).to_string().as_bytes());
}

fn write_number(number: &serde_json::Number, out: &mut Vec<u8>) {
    if let Some(i) = number.as_i64() {
        out.extend_from_slice(i.to_string().as_bytes());
    } else if let Some(u) = number.as_u64() {
        out.extend_from_slice(u.to_string().as_bytes());
    } else if let Some(f) = number.as_f64() {
        // Whole numbers written as floats (1.0) match their integer form.
        if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
            out.extend_from_slice((f as i64).to_string().as_bytes());
        } else {
            out.extend_from_slice(number.to_string().as_bytes());
        }
    }
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => write_number(n, out),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_canonical(&map[key], out);
            }
            out.push(b'}');
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn key(body: Value) -> CacheKey {
        exact_key(Some("pp"), "/api/chat", &body)
    }

    #[test]
    fn order_and_number_forms_dont_matter() {
        let a = key(
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "options": {"temperature": 1, "top_p": 0.9}}),
        );
        let b: Value = serde_json::from_str(
            r#"{"options":{"top_p":0.9,"temperature":1.0},"messages":[{"content":"hi","role":"user"}],"model":"m"}"#,
        )
        .unwrap();
        assert_eq!(a, key(b));
    }

    #[test]
    fn ignored_fields_dont_matter_and_others_do() {
        let base = json!({"model": "m", "messages": [], "stream": true});
        let mut same = base.clone();
        same["stream"] = json!(false);
        same["keep_alive"] = json!("10m");
        same["stream_options"] = json!({"include_usage": true});
        assert_eq!(key(base.clone()), key(same));

        for (field, value) in
            [("think", json!(true)), ("format", json!("json")), ("options", json!({"seed": 1})), ("tools", json!([]))]
        {
            let mut changed = base.clone();
            changed[field] = value;
            assert_ne!(key(base.clone()), key(changed), "{field}");
        }
        let mut other_message = base.clone();
        other_message["messages"] = json!([{"role": "user", "content": "hi"}]);
        assert_ne!(key(base.clone()), key(other_message));
        // An explicit default is still a different request.
        let mut explicit = base.clone();
        explicit["options"] = json!({"temperature": 0.8});
        assert_ne!(key(base), key(explicit));
    }

    #[test]
    fn scope_and_path_matter() {
        let body = json!({"model": "m", "prompt": "hi"});
        assert_ne!(exact_key(Some("a"), "/api/generate", &body), exact_key(Some("b"), "/api/generate", &body));
        assert_ne!(exact_key(None, "/api/generate", &body), exact_key(None, "/v1/completions", &body));
        let exact = exact_key(None, "/api/generate", &body);
        assert_ne!(join_key(&exact, true, false), join_key(&exact, false, false));
        assert_ne!(join_key(&exact, true, false), join_key(&exact, true, true));
    }
}
