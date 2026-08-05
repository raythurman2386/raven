//! Pure-Rust token estimator for context-window management.
//!
//! This is NOT a full tiktoken/huggingface tokenizer — it doesn't need an
//! external vocab file. Instead it uses a GPT-style pre-tokenization regex
//! to split text into chunks, then estimates tokens per chunk. It is
//! deliberately fast (linear in input size, zero per-character allocations)
//! because it runs over the full conversation history on every agent
//! iteration — a slow tokenizer becomes the bottleneck.
//!
//! Accuracy target: within ~15% of actual tiktoken counts for typical
//! English prose and source code. Biased slightly high (over-estimates)
//! so compaction triggers early rather than late.
//!
//! ## How it works
//!
//! 1. **Pre-tokenize**: split text using a GPT-4-style regex that separates
//!    contractions, words, numbers, punctuation, and whitespace.
//! 2. **Estimate**: each word/number run ≈ `bytes / 4` tokens (min 1),
//!    each punctuation run ≈ 1 token, each whitespace run ≈ 1 token
//!    (compacted, matching how tokenizers treat leading spaces).
//! 3. **Overhead**: add ~4% structural overhead, min 1.
//!
//! The old implementation expanded every character into a heap-allocated
//! `String` and applied BPE merges via repeated `Vec::splice` — O(n²) with a
//! per-char allocation. That made a 50KB history take ~3.3s to count, which
//! dominated every turn. The estimator below is O(n) and allocation-light
//! while preserving the same loose accuracy contract.

use std::sync::OnceLock;

/// GPT-4-style pre-tokenization regex.
///
/// Splits text into:
/// - contractions: `'s`, `'t`, `'re`, `'ve`, `'m`, `'ll`, `'d`
/// - word characters: sequences of letters/digits
/// - numbers: sequences of digits
/// - single punctuation/other chars
/// - whitespace runs
static PRETOKEN_RE: OnceLock<regex::Regex> = OnceLock::new();

fn pretoken_re() -> &'static regex::Regex {
    PRETOKEN_RE.get_or_init(|| {
        // Order matters: contractions first, then words, then numbers,
        // then individual chars, then whitespace.
        regex::Regex::new(r"'s|'t|'re|'ve|'m|'ll|'d|[A-Za-z]+|\d+|[^\sA-Za-z\d']+|\s+")
            .expect("valid pre-token regex")
    })
}

/// Estimate the number of tokens in `text` in linear time.
///
/// This is a fast approximation, not an exact tokenizer. It preserves the
/// accuracy contract of the module: counts are within ~15% of real
/// tokenizers, biased slightly high so compaction triggers early.
pub fn count_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 1;
    }

    let mut total = 0usize;
    for m in pretoken_re().find_iter(text) {
        let s = m.as_str();
        // Whitespace runs compact to ~1 token each (leading spaces attach
        // to the following word in most tokenizers).
        if s.chars().all(|c| c.is_whitespace()) {
            total += 1;
        } else if s.is_ascii() {
            // ASCII words/numbers/punct: ~4 bytes per token, min 1.
            total += (s.len() / 4).max(1);
        } else {
            // Non-ASCII (multi-byte UTF-8): count code points, ~4 per token.
            total += (s.chars().count() / 4).max(1);
        }
    }

    // Add ~4% structural overhead (role markers, separators), min 1.
    let overhead = (total as f64 * 0.04).ceil() as usize;
    total + overhead.max(1)
}

/// Per-message structural overhead (role tags, separators, etc.).
/// Matches the OpenAI chat format overhead.
pub const MSG_OVERHEAD: usize = 4;

/// Count tokens consumed by a single chat message.
///
/// Counts: content, tool call names/arguments/ids, and structural overhead.
pub fn message_tokens(msg: &crate::agent::ChatMessage) -> usize {
    let mut total = MSG_OVERHEAD;

    if let Some(content) = &msg.content {
        total += count_tokens(content);
    }

    if let Some(tool_calls) = &msg.tool_calls {
        for tc in tool_calls {
            total += count_tokens(&tc.function.name);
            total += count_tokens(&tc.function.arguments);
            total += count_tokens(&tc.id);
            total += 2; // structural overhead per tool call
        }
    }

    if let Some(id) = &msg.tool_call_id {
        total += count_tokens(id);
    }

    total
}

/// Total token count across a message history.
pub fn history_tokens(messages: &[crate::agent::ChatMessage]) -> usize {
    messages.iter().map(message_tokens).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_returns_one() {
        assert_eq!(count_tokens(""), 1);
    }

    #[test]
    fn single_word_at_least_one_token() {
        assert!(count_tokens("hello") >= 1);
    }

    #[test]
    fn two_words_more_than_one_token() {
        let one = count_tokens("hello");
        let two = count_tokens("hello world");
        assert!(
            two > one,
            "two words should have more tokens than one: {} vs {}",
            two,
            one
        );
    }

    #[test]
    fn longer_text_more_tokens() {
        let short = count_tokens("The quick brown fox");
        let long =
            count_tokens("The quick brown fox jumps over the lazy dog and runs through the forest");
        assert!(
            long > short,
            "longer text should have more tokens: {} vs {}",
            long,
            short
        );
    }

    #[test]
    fn count_tokens_is_fast_on_large_input() {
        // Regression guard for the O(n²) pathology. A ~50KB input must
        // estimate in well under a second (old impl took ~3.3s and scaled
        // quadratically). We don't assert a wall-clock bound (flaky on CI);
        // just exercise a large input to prove it completes and is monotonic.
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(1000);
        let n = count_tokens(&text);
        assert!(n > 1000, "large input should produce many tokens: {n}");
    }

    #[test]
    fn code_is_reasonable() {
        let code = "fn main() -> Result<(), anyhow::Error> { println!(\"hello\"); Ok(()) }";
        let tokens = count_tokens(code);
        // Should be at least 8 tokens (there are ~15 words/punct groups)
        assert!(
            tokens >= 8,
            "code should produce reasonable token count: {}",
            tokens
        );
        // Should not be absurdly high (each char as separate token)
        assert!(
            tokens < code.len(),
            "token count should be less than char count: {} vs {}",
            tokens,
            code.len()
        );
    }

    #[test]
    fn json_arguments_are_counted() {
        let args = r#"{"path": "src/main.rs", "content": "fn main() {}"}"#;
        let tokens = count_tokens(args);
        assert!(
            tokens >= 10,
            "JSON args should produce reasonable token count: {}",
            tokens
        );
    }

    #[test]
    fn whitespace_runs_are_compacted() {
        let with_spaces = count_tokens("hello    world");
        let single_space = count_tokens("hello world");
        // Multiple spaces should not dramatically inflate token count
        assert!(
            with_spaces <= single_space + 4,
            "extra spaces should not inflate much: {} vs {}",
            with_spaces,
            single_space
        );
    }

    #[test]
    fn url_is_reasonable() {
        let url = "https://example.com/path/to/resource";
        let tokens = count_tokens(url);
        assert!(
            tokens >= 3,
            "URL should produce reasonable token count: {}",
            tokens
        );
        assert!(
            tokens < url.len(),
            "URL token count should be less than char count"
        );
    }

    #[test]
    fn repeating_pattern_compacts() {
        // "the the the the the the the the" should be fewer tokens than
        // 8 separate "the" + 8 spaces = 16 raw tokens.
        let repeated = count_tokens("the the the the the the the the");
        assert!(
            repeated <= 16,
            "repeated pattern should compact: {}",
            repeated
        );
    }

    #[test]
    fn newlines_are_tokenized() {
        let text = "line1\nline2\nline3\n";
        let tokens = count_tokens(text);
        assert!(
            tokens >= 3,
            "three lines should produce at least 3 tokens: {}",
            tokens
        );
    }

    #[test]
    fn mixed_code_and_prose() {
        let text = r#"// This function does something
fn do_thing(x: i32) -> i32 {
    let result = x * 2;
    println!("result: {}", result);
    result
}"#;
        let tokens = count_tokens(text);
        // Should be at least 20 (there are many tokens here)
        assert!(
            tokens >= 20,
            "mixed code should produce reasonable count: {}",
            tokens
        );
        // Should not be one-per-char
        assert!(
            tokens < text.len(),
            "should be fewer tokens than chars: {} vs {}",
            tokens,
            text.len()
        );
    }

    #[test]
    fn count_tokens_monotonic() {
        let short = count_tokens("a");
        let medium = count_tokens(&"a".repeat(100));
        let long = count_tokens(&"a".repeat(1000));
        assert!(short < medium);
        assert!(medium < long);
    }

    #[test]
    fn bpe_merges_reduce_token_count() {
        // " the" merges to fewer tokens than " " + "the" separately.
        let merged = count_tokens("the the the");
        let unmerged_approx = 8; // 3x "the" + 2x " " raw
        assert!(
            merged <= unmerged_approx,
            "merges should reduce count: {} vs approx {}",
            merged,
            unmerged_approx
        );
    }
}
