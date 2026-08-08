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

/// Errors that occur during tool execution (filesystem, subprocess, etc.).
///
/// Carries structured context (path, operation, error kind + message) so
/// callers can distinguish transient vs. deterministic failures instead of
/// matching on flat strings.
#[derive(Debug, Error)]
pub enum ToolError {
    /// A filesystem I/O error with path, operation, and preserved message.
    #[error("IO error {operation} '{path}': {message}")]
    Io {
        path: String,
        operation: String,
        kind: std::io::ErrorKind,
        message: String,
    },
    /// Any other tool error (validation, timeout, not-found, subprocess, etc.).
    #[error("{0}")]
    Other(String),
}

impl ToolError {
    /// Create an `Io` variant with path, operation, error kind, and the
    /// original OS error message preserved.
    pub fn io(
        path: impl Into<String>,
        operation: impl Into<String>,
        kind: std::io::ErrorKind,
        message: impl Into<String>,
    ) -> Self {
        ToolError::Io {
            path: path.into(),
            operation: operation.into(),
            kind,
            message: message.into(),
        }
    }

    /// Returns `true` for errors that may succeed on retry (permission
    /// denied, connection failures, timeouts, interrupted syscalls).
    pub fn is_transient(&self) -> bool {
        match self {
            ToolError::Io { kind, .. } => matches!(
                kind,
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
            ),
            ToolError::Other(_) => false,
        }
    }
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
    ///
    /// No longer emitted by the agent loop: a budget-exhausted turn is wrapped
    /// up gracefully with a summary and a `Done` event instead. Retained as an
    /// enum variant for external callers that may still construct it.
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

    #[test]
    fn tool_error_io_carries_path_and_message() {
        let err = ToolError::io(
            "/tmp/test.txt",
            "read_file",
            std::io::ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        );
        let display = err.to_string();
        assert!(display.contains("/tmp/test.txt"), "{display}");
        assert!(display.contains("read_file"), "{display}");
        assert!(display.contains("Permission denied"), "{display}");
        assert!(err.is_transient());
    }

    #[test]
    fn tool_error_other_is_not_transient() {
        let err = ToolError::Other("validation failed".into());
        assert!(!err.is_transient());
    }

    #[test]
    fn tool_error_io_not_found_is_transient() {
        let err = ToolError::io(
            "/tmp/missing",
            "read_file",
            std::io::ErrorKind::NotFound,
            "No such file",
        );
        assert!(err.is_transient());
    }

    #[test]
    fn tool_error_io_broken_pipe_is_not_transient() {
        let err = ToolError::io(
            "/dev/null",
            "write_file",
            std::io::ErrorKind::BrokenPipe,
            "Broken pipe",
        );
        assert!(!err.is_transient());
    }
}
