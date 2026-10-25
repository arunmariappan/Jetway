//! The two APIs Jetway speaks: Ollama's own and Ollama's OpenAI-compatible one (plan D4). Jetway never translates
//! between them; it only needs to know which one a request uses and how its responses are framed.

/// Which API a request uses. OpenAI-compatible paths start with `/v1/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Ollama,
    OpenAi,
}

impl Protocol {
    pub fn for_path(path: &str) -> Self {
        if path == "/v1" || path.starts_with("/v1/") { Self::OpenAi } else { Self::Ollama }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::OpenAi => "openai",
        }
    }
}

/// How a response body is framed, from its `Content-Type` (spike S1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `application/x-ndjson`: one JSON object per line (Ollama's streams).
    Ndjson,
    /// `text/event-stream`: `data: {...}` events separated by blank lines (OpenAI-compatible streams).
    Sse,
    /// Anything else, such as one JSON body.
    Whole,
}

impl Framing {
    pub fn from_content_type(content_type: Option<&str>) -> Self {
        let Some(content_type) = content_type else {
            return Self::Whole;
        };
        let essence = content_type.split(';').next().unwrap_or_default().trim();
        if essence.eq_ignore_ascii_case("application/x-ndjson") {
            Self::Ndjson
        } else if essence.eq_ignore_ascii_case("text/event-stream") {
            Self::Sse
        } else {
            Self::Whole
        }
    }

    pub fn is_stream(self) -> bool {
        !matches!(self, Self::Whole)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_by_path() {
        assert_eq!(Protocol::for_path("/v1/chat/completions"), Protocol::OpenAi);
        assert_eq!(Protocol::for_path("/v1"), Protocol::OpenAi);
        assert_eq!(Protocol::for_path("/api/chat"), Protocol::Ollama);
        assert_eq!(Protocol::for_path("/v10/x"), Protocol::Ollama);
        assert_eq!(Protocol::for_path("/"), Protocol::Ollama);
    }

    #[test]
    fn framing_by_content_type() {
        assert_eq!(Framing::from_content_type(Some("application/x-ndjson")), Framing::Ndjson);
        assert_eq!(Framing::from_content_type(Some("text/event-stream; charset=utf-8")), Framing::Sse);
        assert_eq!(Framing::from_content_type(Some("application/json; charset=utf-8")), Framing::Whole);
        assert_eq!(Framing::from_content_type(None), Framing::Whole);
    }
}
