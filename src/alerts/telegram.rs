//! Alert notifications through a Telegram bot (phase 9.5). The bot token and chat id come from
//! `JETWAY_TELEGRAM_TOKEN` and `JETWAY_TELEGRAM_CHAT_ID`, in the environment or a `.env` file in the working
//! directory. The token is part of the request URL, so it is never logged: errors are reported without their URL,
//! and any text that might hold the token has it masked.

use std::time::Duration;

const TOKEN: &str = "JETWAY_TELEGRAM_TOKEN";
const CHAT_ID: &str = "JETWAY_TELEGRAM_CHAT_ID";

/// Sends messages to one chat. Not `Debug`, so the token can't end up in a log by accident.
pub struct Telegram {
    client: reqwest::Client,
    api: String,
    token: String,
    chat_id: String,
}

/// A variable from the environment, or else from `.env`.
fn setting(name: &str) -> Option<String> {
    if let Ok(value) = std::env::var(name) {
        return Some(value).filter(|v| !v.trim().is_empty());
    }
    let text = std::fs::read_to_string(".env").ok()?;
    dotenv_value(&text, name)
}

/// `NAME=value` from a `.env` file's text; `export`, quotes and `#` comment lines are allowed.
fn dotenv_value(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=')?;
        if line.starts_with('#') || key.trim() != name {
            return None;
        }
        let value = value.trim();
        let unquoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        Some(unquoted.to_string()).filter(|v| !v.is_empty())
    })
}

impl Telegram {
    /// `None`, with the reason, when the token or chat id is missing.
    pub fn from_env(api: &str) -> Result<Self, String> {
        let token = setting(TOKEN).ok_or_else(|| format!("{TOKEN} isn't set"))?;
        let chat_id = setting(CHAT_ID).ok_or_else(|| format!("{CHAT_ID} isn't set"))?;
        Self::new(api, token, chat_id)
    }

    pub fn new(api: &str, token: String, chat_id: String) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("couldn't build the Telegram client: {}", e.without_url()))?;
        Ok(Self { client, api: api.trim_end_matches('/').to_string(), token, chat_id })
    }

    fn mask(&self, text: &str) -> String {
        if self.token.is_empty() { text.to_string() } else { text.replace(&self.token, "***") }
    }

    pub async fn send(&self, text: &str) -> Result<(), String> {
        let url = format!("{}/bot{}/sendMessage", self.api, self.token);
        let body = serde_json::json!({ "chat_id": self.chat_id, "text": format!("Jetway {text}") });
        let response = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| self.mask(&format!("couldn't reach Telegram: {}", e.without_url())))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let detail = response.text().await.unwrap_or_default();
        Err(self.mask(&format!("Telegram answered {status}: {}", detail.chars().take(200).collect::<String>())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_lines() {
        let text = "# alerts\nexport JETWAY_TELEGRAM_TOKEN=\"123:abc\"\nJETWAY_TELEGRAM_CHAT_ID = 42\nOTHER=x\n";
        assert_eq!(dotenv_value(text, TOKEN).as_deref(), Some("123:abc"));
        assert_eq!(dotenv_value(text, CHAT_ID).as_deref(), Some("42"));
        assert_eq!(dotenv_value("#JETWAY_TELEGRAM_TOKEN=x", TOKEN), None);
        assert_eq!(dotenv_value("JETWAY_TELEGRAM_TOKEN=", TOKEN), None);
    }
}
