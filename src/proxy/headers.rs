//! Which headers cross Jetway (phase 1.5). Hop-by-hop headers belong to one connection and never cross a proxy;
//! `Authorization` carries Jetway's app keys, not Ollama's (Ollama has none); `Accept-Encoding` is dropped so the
//! upstream answers uncompressed and the recorder can read what it forwards.

use axum::http::header::{self, HeaderMap, HeaderName};

const HOP_BY_HOP: [HeaderName; 8] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Headers named in `Connection` are hop-by-hop too.
fn connection_listed(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect()
}

fn without(headers: &HeaderMap, drop: impl Fn(&HeaderName) -> bool) -> HeaderMap {
    let listed = connection_listed(headers);
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if !HOP_BY_HOP.contains(name) && !listed.contains(name) && !drop(name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Headers sent upstream. `Host` and `Content-Length` are set by the client library from the URL and body.
pub fn to_upstream(incoming: &HeaderMap) -> HeaderMap {
    without(incoming, |name| {
        name == header::HOST
            || name == header::CONTENT_LENGTH
            || name == header::AUTHORIZATION
            || name == header::ACCEPT_ENCODING
            || name.as_str().starts_with("x-jetway-")
    })
}

/// Headers sent back to the client. The server adds its own `Date`.
pub fn to_client(upstream: &HeaderMap) -> HeaderMap {
    without(upstream, |name| name == header::DATE)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn map(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn upstream_requests_lose_hop_by_hop_auth_and_jetway_headers() {
        let incoming = map(&[
            ("host", "127.0.0.1:8300"),
            ("connection", "keep-alive, x-custom-hop"),
            ("x-custom-hop", "1"),
            ("keep-alive", "timeout=5"),
            ("authorization", "Bearer jw-key"),
            ("accept-encoding", "gzip"),
            ("content-length", "12"),
            ("x-jetway-app", "paperpilot"),
            ("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
            ("content-type", "application/json"),
            ("user-agent", "OllamaSharp"),
        ]);
        let out = to_upstream(&incoming);
        let names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["traceparent", "content-type", "user-agent"]);
    }

    #[test]
    fn client_responses_lose_hop_by_hop_and_date() {
        let upstream = map(&[
            ("content-type", "application/x-ndjson"),
            ("transfer-encoding", "chunked"),
            ("date", "Sun, 25 Oct 2026 12:24:55 GMT"),
            ("content-length", "327"),
        ]);
        let out = to_client(&upstream);
        let names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["content-type", "content-length"]);
    }
}
