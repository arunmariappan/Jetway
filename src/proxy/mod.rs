//! Forwarding to the upstream (phase 1): the HTTP client, header rules, the stream tee, the recorder and error
//! bodies in each protocol.

pub mod errors;
pub mod headers;
pub mod record;
pub mod tee;

use std::time::Duration;

use crate::config::UpstreamConfig;

/// The upstream Ollama and the client that talks to it.
pub struct Upstream {
    pub client: reqwest::Client,
    /// The configured URL without a trailing slash; request paths are appended to it.
    base: String,
    pub first_byte_timeout: Duration,
    pub idle_timeout: Duration,
}

impl Upstream {
    pub fn new(config: &UpstreamConfig) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(64)
            // Proxy settings in the environment must never redirect calls to the local upstream.
            .no_proxy()
            .build()?;
        Ok(Self {
            client,
            base: config.url.trim_end_matches('/').to_string(),
            first_byte_timeout: Duration::from_secs(config.first_byte_timeout_seconds),
            idle_timeout: Duration::from_secs(config.idle_timeout_seconds),
        })
    }

    /// The upstream URL for a path (starting with `/`, query included).
    pub fn url_for(&self, path_and_query: &str) -> String {
        format!("{}{path_and_query}", self.base)
    }

    pub fn base(&self) -> &str {
        &self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_join_without_double_slashes() {
        let config = UpstreamConfig {
            url: "http://127.0.0.1:8210/a/opsdesk/".into(),
            ..UpstreamConfig::default()
        };
        let upstream = Upstream::new(&config).unwrap();
        assert_eq!(
            upstream.url_for("/api/chat?x=1"),
            "http://127.0.0.1:8210/a/opsdesk/api/chat?x=1"
        );
        let upstream = Upstream::new(&UpstreamConfig::default()).unwrap();
        assert_eq!(upstream.url_for("/"), "http://127.0.0.1:11434/");
    }
}
