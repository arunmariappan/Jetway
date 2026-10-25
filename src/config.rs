//! Configuration: defaults, then `jetway.toml`, then `JETWAY__*` environment variables (`__` separates sections,
//! so `JETWAY__SERVER__PORT=9000` sets `server.port`). Unknown keys are errors, so typos don't pass silently.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use reqwest::Url;
use serde::{Deserialize, Serialize};

pub const ENV_PREFIX: &str = "JETWAY__";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Loopback only: Jetway has no authentication (plan D17).
    pub host: IpAddr,
    /// Gateway: Ollama and OpenAI-compatible APIs.
    pub port: u16,
    /// Dashboard, admin API and `/metrics`.
    pub admin_port: u16,
    /// Requests are read in full because Jetway parses them; image inputs are base64.
    pub max_request_bytes: usize,
    /// How long in-flight streams may finish after Ctrl+C.
    pub shutdown_grace_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8300,
            admin_port: 8301,
            max_request_bytes: 64 * 1024 * 1024,
            shutdown_grace_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    pub url: String,
    pub connect_timeout_ms: u64,
    /// Long prompts on a busy GPU can take minutes before the first byte.
    pub first_byte_timeout_seconds: u64,
    /// The longest silence allowed between two stream chunks.
    pub idle_timeout_seconds: u64,
    /// Requests sent upstream at once (phase 9's scheduler).
    pub slots: u32,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            name: "ollama".into(),
            url: "http://127.0.0.1:11434".into(),
            connect_timeout_ms: 2000,
            first_byte_timeout_seconds: 600,
            idle_timeout_seconds: 120,
            slots: 1,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file {} doesn't exist", .0.display())]
    Missing(PathBuf),
    #[error(transparent)]
    Load(#[from] Box<figment::Error>),
    #[error("invalid configuration:\n{}", bullet_list(.0))]
    Invalid(Vec<String>),
}

fn bullet_list(problems: &[String]) -> String {
    problems
        .iter()
        .map(|p| format!("  - {p}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Config {
    /// Loads `path` (which must exist when `required`), applies environment overrides and validates the result.
    pub fn load(path: &Path, required: bool) -> Result<Self, ConfigError> {
        if required && !path.exists() {
            return Err(ConfigError::Missing(path.to_path_buf()));
        }
        Self::from_figment(Self::figment(path))
    }

    pub fn figment(path: &Path) -> Figment {
        Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed(ENV_PREFIX).split("__"))
    }

    pub fn from_figment(figment: Figment) -> Result<Self, ConfigError> {
        let config: Config = figment.extract().map_err(Box::new)?;
        config.validate()?;
        Ok(config)
    }

    /// Checks everything that deserialising can't, and reports all problems at once.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut problems = Vec::new();
        let server = &self.server;
        if !server.host.is_loopback() {
            problems.push(format!(
                "server.host is {}, but Jetway listens on loopback addresses only (it has no authentication)",
                server.host
            ));
        }
        if server.port == 0 || server.admin_port == 0 {
            problems.push("server.port and server.admin_port must not be 0".into());
        }
        if server.port == server.admin_port {
            problems.push(format!("server.port and server.admin_port are both {}", server.port));
        }
        if server.max_request_bytes < 1024 {
            problems.push("server.max_request_bytes must be at least 1024".into());
        }

        let upstream = &self.upstream;
        if !is_valid_name(&upstream.name) {
            problems.push(format!(
                "upstream.name {:?} must match [a-z0-9-]{{1,32}}",
                upstream.name
            ));
        }
        if let Err(problem) = check_upstream_url(&upstream.url) {
            problems.push(format!("upstream.url {:?}: {problem}", upstream.url));
        }
        if upstream.connect_timeout_ms == 0
            || upstream.first_byte_timeout_seconds == 0
            || upstream.idle_timeout_seconds == 0
        {
            problems.push("upstream timeouts must be greater than 0".into());
        }
        if upstream.slots == 0 {
            problems.push("upstream.slots must be at least 1".into());
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(problems))
        }
    }
}

/// App and upstream names: lowercase letters, digits and hyphens, 1 to 32 characters.
pub fn is_valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn check_upstream_url(url: &str) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|e| e.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("the scheme must be http or https".into());
    }
    if parsed.host_str() == Some("localhost") {
        return Err(
            "use 127.0.0.1 instead of localhost: Windows tries ::1 first, and a refused IPv6 connection \
                    costs about 2 seconds"
                .into(),
        );
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("it must not have a query or fragment".into());
    }
    Ok(())
}

#[cfg(test)]
// `Jail::expect_with` fixes the closure's error type to `figment::Error`, which is large.
#[allow(clippy::result_large_err)]
mod tests {
    use figment::Jail;

    use super::*;

    fn load_in(jail: &Jail) -> Result<Config, ConfigError> {
        Config::load(&jail.directory().join("jetway.toml"), false)
    }

    fn problems(result: Result<Config, ConfigError>) -> Vec<String> {
        match result {
            Err(ConfigError::Invalid(problems)) => problems,
            other => panic!("expected validation problems, got {other:?}"),
        }
    }

    #[test]
    fn defaults_are_valid_and_match_the_plan() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.server.host.to_string(), "127.0.0.1");
        assert_eq!((config.server.port, config.server.admin_port), (8300, 8301));
        assert_eq!(config.upstream.url, "http://127.0.0.1:11434");
    }

    #[test]
    fn the_committed_jetway_toml_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("jetway.toml");
        let config =
            Config::from_figment(Figment::from(Serialized::defaults(Config::default())).merge(Toml::file(&path)))
                .unwrap();
        assert_eq!(config, Config::default(), "jetway.toml should state the defaults");
    }

    #[test]
    fn file_then_environment() {
        Jail::expect_with(|jail| {
            jail.create_file("jetway.toml", "[server]\nport = 9000\n\n[upstream]\nslots = 2\n")?;
            jail.set_env("JETWAY__SERVER__PORT", "9100");
            let config = load_in(jail).unwrap();
            assert_eq!(config.server.port, 9100);
            assert_eq!(config.upstream.slots, 2);
            assert_eq!(config.server.admin_port, 8301);
            Ok(())
        });
    }

    #[test]
    fn unknown_keys_are_errors() {
        Jail::expect_with(|jail| {
            jail.create_file("jetway.toml", "[server]\nprot = 9000\n")?;
            assert!(matches!(load_in(jail), Err(ConfigError::Load(_))));
            Ok(())
        });
    }

    #[test]
    fn a_required_file_must_exist() {
        let missing = Path::new("does-not-exist/jetway.toml");
        assert!(matches!(Config::load(missing, true), Err(ConfigError::Missing(_))));
    }

    #[test]
    fn localhost_upstream_is_refused_with_a_hint() {
        Jail::expect_with(|jail| {
            jail.create_file("jetway.toml", "[upstream]\nurl = \"http://localhost:11434\"\n")?;
            let problems = problems(load_in(jail));
            assert_eq!(problems.len(), 1);
            assert!(
                problems[0].contains("use 127.0.0.1 instead of localhost"),
                "{problems:?}"
            );
            Ok(())
        });
    }

    #[test]
    fn all_problems_are_reported_together() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "jetway.toml",
                "[server]\nhost = \"0.0.0.0\"\nport = 8301\n\n[upstream]\nname = \"Ollama\"\nurl = \"ftp://x\"\nslots = 0\n",
            )?;
            let problems = problems(load_in(jail));
            assert_eq!(problems.len(), 5, "{problems:#?}");
            Ok(())
        });
    }

    #[test]
    fn names() {
        assert!(is_valid_name("paperpilot"));
        assert!(is_valid_name("ops-desk-2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("PaperPilot"));
        assert!(!is_valid_name("a_b"));
        assert!(!is_valid_name(&"a".repeat(33)));
    }
}
