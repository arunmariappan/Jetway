//! Which app a request comes from (plan D12): the `/a/{app}/` path prefix, else the `X-Jetway-App` header, else a
//! bearer key listed under `[apps.<name>] keys`, else `default`.

use std::collections::HashMap;

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;

use crate::config::{Config, is_valid_name};

pub const APP_HEADER: &str = "x-jetway-app";
pub const DEFAULT_APP: &str = "default";
const PREFIX: &str = "/a/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Prefix,
    Header,
    Key,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub app: String,
    pub source: Source,
    /// The request path with any `/a/{app}` prefix removed; always starts with `/`.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("app name {0:?} must match [a-z0-9-]{{1,32}}")]
    InvalidName(String),
}

/// Maps bearer keys to app names, built once from the configuration.
#[derive(Debug, Clone, Default)]
pub struct KeyMap(HashMap<String, String>);

impl KeyMap {
    pub fn from_config(config: &Config) -> Self {
        let keys = config
            .apps
            .iter()
            .flat_map(|(app, c)| c.keys.iter().map(move |k| (k.clone(), app.clone())));
        Self(keys.collect())
    }

    fn app_for(&self, headers: &HeaderMap) -> Option<&str> {
        let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
        let (scheme, key) = value.trim().split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        self.0.get(key.trim()).map(String::as_str)
    }
}

pub fn identify(path: &str, headers: &HeaderMap, keys: &KeyMap) -> Result<Identity, IdentityError> {
    if let Some(rest) = path.strip_prefix(PREFIX) {
        let (app, tail) = rest.split_once('/').map_or((rest, ""), |(app, tail)| (app, tail));
        if !is_valid_name(app) {
            return Err(IdentityError::InvalidName(app.to_string()));
        }
        // `/a/{app}` and `/a/{app}/` both mean `/`: OllamaSharp checks "is Ollama running" with a GET of its base
        // address (spike S4).
        return Ok(Identity {
            app: app.to_string(),
            source: Source::Prefix,
            path: format!("/{tail}"),
        });
    }

    let path = path.to_string();
    if let Some(value) = headers.get(APP_HEADER) {
        let app = value.to_str().unwrap_or_default().trim();
        if !is_valid_name(app) {
            return Err(IdentityError::InvalidName(app.to_string()));
        }
        return Ok(Identity {
            app: app.to_string(),
            source: Source::Header,
            path,
        });
    }
    if let Some(app) = keys.app_for(headers) {
        return Ok(Identity {
            app: app.to_string(),
            source: Source::Key,
            path,
        });
    }
    Ok(Identity {
        app: DEFAULT_APP.to_string(),
        source: Source::Default,
        path,
    })
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;
    use crate::config::AppConfig;

    fn keys() -> KeyMap {
        let mut config = Config::default();
        config.apps.insert(
            "chatui".into(),
            AppConfig {
                keys: vec!["jw-chat".into()],
                ..AppConfig::default()
            },
        );
        KeyMap::from_config(&config)
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn prefix_wins_and_is_stripped() {
        let h = headers(&[(APP_HEADER, "other"), ("authorization", "Bearer jw-chat")]);
        let id = identify("/a/paperpilot/api/chat", &h, &keys()).unwrap();
        assert_eq!(
            (id.app.as_str(), id.source, id.path.as_str()),
            ("paperpilot", Source::Prefix, "/api/chat")
        );
    }

    #[test]
    fn bare_prefix_means_root() {
        for path in ["/a/paperpilot", "/a/paperpilot/"] {
            let id = identify(path, &HeaderMap::new(), &keys()).unwrap();
            assert_eq!(id.path, "/", "{path}");
        }
    }

    #[test]
    fn header_then_key_then_default() {
        let k = keys();
        let id = identify("/api/chat", &headers(&[(APP_HEADER, "opsdesk")]), &k).unwrap();
        assert_eq!((id.app.as_str(), id.source), ("opsdesk", Source::Header));
        let id = identify("/v1/models", &headers(&[("authorization", "bearer  jw-chat ")]), &k).unwrap();
        assert_eq!((id.app.as_str(), id.source), ("chatui", Source::Key));
        let id = identify("/api/chat", &headers(&[("authorization", "Bearer unknown")]), &k).unwrap();
        assert_eq!((id.app.as_str(), id.source), (DEFAULT_APP, Source::Default));
    }

    #[test]
    fn invalid_names_are_errors() {
        assert!(identify("/a/Paper_Pilot/api/chat", &HeaderMap::new(), &keys()).is_err());
        assert!(identify("/a//api/chat", &HeaderMap::new(), &keys()).is_err());
        assert!(identify("/api/chat", &headers(&[(APP_HEADER, "")]), &keys()).is_err());
    }
}
