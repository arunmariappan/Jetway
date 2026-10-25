//! The path allow-list (plan D4, phase 1.4): a client can reach only what Jetway knows.

/// What a request path is, after any `/a/{app}` prefix has been removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    /// Chat and completion endpoints of either API.
    Generation,
    Embedding,
    /// Model lists, model details, version, and Ollama's "is running" root.
    Info,
    /// Ollama's model management; forwarded only for apps marked `admin`.
    ModelManagement,
    Unknown,
}

impl PathClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generation => "generation",
            Self::Embedding => "embedding",
            Self::Info => "info",
            Self::ModelManagement => "model_management",
            Self::Unknown => "unknown",
        }
    }
}

pub fn classify(path: &str) -> PathClass {
    match path {
        "/api/chat" | "/api/generate" | "/v1/chat/completions" | "/v1/completions" => PathClass::Generation,
        "/api/embed" | "/api/embeddings" | "/v1/embeddings" => PathClass::Embedding,
        "/" | "/api/tags" | "/api/ps" | "/api/version" | "/api/show" | "/v1/models" => PathClass::Info,
        "/api/pull" | "/api/push" | "/api/create" | "/api/copy" | "/api/delete" => PathClass::ModelManagement,
        _ if is_single_segment_under(path, "/api/blobs/") => PathClass::ModelManagement,
        _ if is_single_segment_under(path, "/v1/models/") => PathClass::Info,
        _ => PathClass::Unknown,
    }
}

fn is_single_segment_under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list() {
        for path in ["/api/chat", "/api/generate", "/v1/chat/completions", "/v1/completions"] {
            assert_eq!(classify(path), PathClass::Generation, "{path}");
        }
        for path in ["/api/embed", "/api/embeddings", "/v1/embeddings"] {
            assert_eq!(classify(path), PathClass::Embedding, "{path}");
        }
        for path in ["/", "/api/tags", "/api/ps", "/api/version", "/api/show", "/v1/models", "/v1/models/qwen3.5:4b"] {
            assert_eq!(classify(path), PathClass::Info, "{path}");
        }
        for path in ["/api/pull", "/api/delete", "/api/create", "/api/copy", "/api/push", "/api/blobs/sha256:abc"] {
            assert_eq!(classify(path), PathClass::ModelManagement, "{path}");
        }
        for path in ["/api/chat/", "/api", "/admin", "/api/blobs/", "/api/blobs/a/b", "/v1/models/a/b", "/metrics"] {
            assert_eq!(classify(path), PathClass::Unknown, "{path}");
        }
    }
}
