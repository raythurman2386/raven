use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Real token usage reported by the provider for one request.
///
/// OpenAI-compatible endpoints return this in the `usage` field of a
/// non-streaming response, or in the final streaming chunk (an empty-`choices`
/// chunk sent when the request sets `stream_options: {"include_usage": true}`).
/// Ollama, llama.cpp-server, and vLLM all emit these fields on their
/// OpenAI-compatible endpoints.
///
/// Serialized onto persisted assistant messages (camelCase, matching the
/// collector's record format) so external tools read the provider's real
/// meters; deserialization also accepts the provider's snake_case keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(alias = "prompt_tokens")]
    pub prompt_tokens: u64,
    #[serde(alias = "completion_tokens")]
    pub completion_tokens: u64,
    #[serde(alias = "total_tokens")]
    pub total_tokens: u64,
    /// Prompt tokens served from the provider's prefix cache.
    ///
    /// Read from `usage.prompt_tokens_details.cached_tokens` (xAI / OpenAI).
    /// Older payloads omit it; those deserialize as 0.
    #[serde(default, alias = "cached_tokens")]
    pub cached_tokens: u64,
    /// Reasoning tokens included in `completion_tokens` (billed as output).
    ///
    /// Read from `usage.completion_tokens_details.reasoning_tokens`. Not added
    /// on top of `completion_tokens` when estimating cost.
    #[serde(default, alias = "reasoning_tokens")]
    pub reasoning_tokens: u64,
}

impl TokenUsage {
    /// Extract usage from a response (or streaming chunk) JSON value.
    ///
    /// Returns `None` when the payload carries no usable `usage` object —
    /// older Ollama builds, strict proxies, or a chunk that is pure content —
    /// so callers fall back to the estimator. A `usage` with a missing or
    /// zero `prompt_tokens` is treated as absent: every real prompt costs at
    /// least one token, so zero means the field is a placeholder.
    pub fn from_json(v: &Value) -> Option<Self> {
        let u = v.get("usage")?;
        let prompt_tokens = u.get("prompt_tokens")?.as_u64()?;
        if prompt_tokens == 0 {
            return None;
        }
        let cached_tokens = u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|n| n.as_u64())
            .or_else(|| u.get("cached_tokens").and_then(|n| n.as_u64()))
            .unwrap_or(0)
            .min(prompt_tokens);
        let reasoning_tokens = u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|n| n.as_u64())
            .or_else(|| u.get("reasoning_tokens").and_then(|n| n.as_u64()))
            .unwrap_or(0);
        Some(Self {
            prompt_tokens,
            completion_tokens: u
                .get("completion_tokens")
                .and_then(|c| c.as_u64())
                .unwrap_or(0),
            total_tokens: u
                .get("total_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(prompt_tokens),
            cached_tokens,
            reasoning_tokens,
        })
    }

    /// Prompt tokens billed at the uncached input rate.
    pub fn uncached_input(self) -> u64 {
        self.prompt_tokens.saturating_sub(self.cached_tokens)
    }
}
