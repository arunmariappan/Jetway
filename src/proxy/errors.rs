//! Error bodies in the client's own protocol (phase 1.8). Ollama answers `{"error":"..."}`; its OpenAI-compatible
//! API answers `{"error":{"message","type","param","code"}}` (spike S1). Jetway writes its own errors the same way.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header::CONTENT_TYPE};
use axum::response::Response;
use bytes::Bytes;
use serde_json::json;

use crate::protocol::{Framing, Protocol};

/// The OpenAI `type` for a status, following Ollama's names where it has one.
fn openai_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 405 | 413 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        502 | 503 => "upstream_unavailable",
        504 => "upstream_timeout",
        _ => "api_error",
    }
}

fn error_value(protocol: Protocol, status: StatusCode, message: &str) -> serde_json::Value {
    match protocol {
        Protocol::Ollama => json!({ "error": message }),
        Protocol::OpenAi => json!({
            "error": { "message": message, "type": openai_type(status), "param": null, "code": null }
        }),
    }
}

pub fn error_body(protocol: Protocol, status: StatusCode, message: &str) -> Bytes {
    Bytes::from(error_value(protocol, status, message).to_string())
}

/// A complete error response, for failures before any upstream byte reached the client.
pub fn error_response(protocol: Protocol, status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(error_body(protocol, status, message)));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    response
}

/// The last piece of a stream that failed midway, in the stream's framing, as Ollama itself ends a failed stream.
/// `tail` holds the last bytes already sent (at most two), so a partial line is closed first. A whole-body response
/// can't carry an error after its start, so it gets nothing and simply ends early.
pub fn stream_error(framing: Framing, protocol: Protocol, tail: &[u8], message: &str) -> Option<Bytes> {
    let value = error_value(protocol, StatusCode::BAD_GATEWAY, message);
    let (separator, line) = match framing {
        Framing::Whole => return None,
        Framing::Ndjson => {
            let separator: &[u8] = if tail.is_empty() || tail.ends_with(b"\n") { b"" } else { b"\n" };
            (separator, format!("{value}\n"))
        }
        Framing::Sse => {
            let separator: &[u8] = if tail.is_empty() || tail.ends_with(b"\n\n") {
                b""
            } else if tail.ends_with(b"\n") {
                b"\n"
            } else {
                b"\n\n"
            };
            (separator, format!("data: {value}\n\n"))
        }
    };
    let mut bytes = Vec::with_capacity(separator.len() + line.len());
    bytes.extend_from_slice(separator);
    bytes.extend_from_slice(line.as_bytes());
    Some(Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_and_openai_bodies() {
        let ollama = error_body(Protocol::Ollama, StatusCode::NOT_FOUND, "Jetway doesn't forward /x");
        assert_eq!(ollama, r#"{"error":"Jetway doesn't forward /x"}"#);
        let openai = error_body(Protocol::OpenAi, StatusCode::BAD_GATEWAY, "Ollama isn't reachable");
        assert_eq!(
            openai,
            r#"{"error":{"message":"Ollama isn't reachable","type":"upstream_unavailable","param":null,"code":null}}"#
        );
    }

    #[test]
    fn stream_errors_close_partial_lines() {
        let ndjson = |tail: &[u8]| stream_error(Framing::Ndjson, Protocol::Ollama, tail, "boom").unwrap();
        assert_eq!(ndjson(b"}\n"), "{\"error\":\"boom\"}\n");
        assert_eq!(ndjson(b""), "{\"error\":\"boom\"}\n");
        assert_eq!(ndjson(b"\"x"), "\n{\"error\":\"boom\"}\n");

        let sse = |tail: &[u8]| stream_error(Framing::Sse, Protocol::OpenAi, tail, "boom").unwrap();
        assert!(sse(b"\n\n").starts_with(b"data: {"));
        assert!(sse(b"}\n").starts_with(b"\ndata: {"));
        assert!(sse(b"}}").starts_with(b"\n\ndata: {"));
        assert!(sse(b"").ends_with(b"}}\n\n"));

        assert!(stream_error(Framing::Whole, Protocol::Ollama, b"{", "boom").is_none());
    }
}
