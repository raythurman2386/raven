//! Typed errors for the agent loop.
//!
//! Structured variants so the agent, headless runner, and TUI can produce
//! clear diagnostics. Transient errors (connection refused, 5xx, 429) are
//! retried with backoff; deterministic errors (404, 400) fail immediately.

use thiserror::Error;

const MAX_HTTP_ERROR_BODY_LEN: usize = 500;

/// Shorten an HTTP error body to a capped length, appending an ellipsis if
/// truncation occurred, so large error responses don't waste context.
pub fn cap_http_body(body: String) -> String {
    if body.len() <= MAX_HTTP_ERROR_BODY_LEN {
        return body;
    }
    let mut truncated = body;
    truncated.truncate(MAX_HTTP_ERROR_BODY_LEN);
    truncated.push('…');
    truncated
}

/// Errors that can occur during the agent loop.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Could not connect to the Ollama endpoint (it may not be running).
    #[error("Ollama unreachable at {url}: {source}")]
    OllamaUnreachable {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// The model name was not found on the server. The user should pull it.
    #[error("Model '{model}' not found. Pull it with: ollama pull {model}")]
    ModelNotFound { model: String },

    /// A non-retryable HTTP error (4xx other than 429).
    #[error("HTTP {status} from Ollama: {body}")]
    HttpError { status: u16, body: String },

    /// The agent exhausted its iteration budget without finishing.
    #[error("Max iterations ({0}) reached without completion")]
    MaxIterations(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_http_body_short_body_unchanged() {
        let body = "short error".to_string();
        assert_eq!(cap_http_body(body.clone()), body);
    }

    #[test]
    fn cap_http_body_exact_limit_unchanged() {
        let body = "x".repeat(MAX_HTTP_ERROR_BODY_LEN);
        assert_eq!(cap_http_body(body.clone()), body);
    }

    #[test]
    fn cap_http_body_long_body_truncated() {
        let body = "x".repeat(MAX_HTTP_ERROR_BODY_LEN + 100);
        let capped = cap_http_body(body);
        assert_eq!(capped.len(), MAX_HTTP_ERROR_BODY_LEN + 3); // 3 bytes for "…"
        assert!(capped.ends_with('…'));
        assert!(capped.starts_with(&"x".repeat(MAX_HTTP_ERROR_BODY_LEN)));
    }

    #[test]
    fn cap_http_body_empty_body_unchanged() {
        let body = String::new();
        assert_eq!(cap_http_body(body.clone()), body);
    }
}
