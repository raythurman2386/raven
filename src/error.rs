//! Typed errors for the agent loop.
//!
//! Structured variants so the agent, headless runner, and TUI can produce
//! clear diagnostics. Transient errors (connection refused, 5xx, 429) are
//! retried with backoff; deterministic errors (404, 400) fail immediately.

use thiserror::Error;

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
