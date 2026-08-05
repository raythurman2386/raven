//! Pure-Rust BPE-like tokenizer for accurate token estimation.
//!
//! This is NOT a full tiktoken/huggingface tokenizer — it doesn't need an
//! external vocab file. Instead it uses a GPT-style pre-tokenization regex
//! to split text into chunks, then applies a hand-curated BPE merge table
//! covering common English and code patterns. The result is significantly
//! more accurate than the old ~3-chars/token heuristic while remaining
//! zero-dependency and fast.
//!
//! Accuracy target: within ~15% of actual tiktoken counts for typical
//! English prose and source code. Biased slightly high (over-estimates)
//! so compaction triggers early rather than late.
//!
//! ## How it works
//!
//! 1. **Pre-tokenize**: split text using a GPT-4-style regex that separates
//!    contractions, words, numbers, punctuation, and whitespace.
//! 2. **BPE merges**: apply a fixed merge table (ranked by frequency) to
//!    each pre-token, merging adjacent byte pairs into longer tokens.
//! 3. **Count**: the number of tokens after merging is the estimate.
//!
//! ## Limitations
//!
//! - No support for non-English text beyond UTF-8 byte fallbacks.
//! - Merge table is hand-curated, not learned from a corpus.
//! - Does not exactly match any specific model's tokenizer (GPT, Llama, etc.)
//!   but is close enough for context-window management.

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

/// A BPE merge rule: merge pair (left, right) → combined token.
///
/// Ranked by priority — earlier entries are applied first.
struct BpeMerge {
    left: &'static str,
    right: &'static str,
}

/// Hand-curated BPE merge table.
///
/// Ordered by approximate frequency in English text + source code.
/// Each entry merges two adjacent tokens into one, reducing the token count
/// by 1. The table is applied iteratively until no more merges apply.
///
/// This covers:
/// - Common English word pairs (" the", " and", " to", " of", " in")
/// - Common suffixes ("ing", "ed", "ly", "tion", "ment")
/// - Code patterns ("fn ", "let ", "pub ", " =>", "()")
/// - Common prefixes (" re", " pre", " un", " dis")
static MERGES: &[BpeMerge] = &[
    // ── Build common words from characters (bottom-up) ─────────────────
    // These must come first so that "the", "and", etc. exist as tokens
    // before the space+word merges try to match them.
    BpeMerge {
        left: "t",
        right: "h",
    },
    BpeMerge {
        left: "th",
        right: "e",
    },
    BpeMerge {
        left: "a",
        right: "n",
    },
    BpeMerge {
        left: "an",
        right: "d",
    },
    BpeMerge {
        left: "i",
        right: "n",
    },
    BpeMerge {
        left: "t",
        right: "o",
    },
    BpeMerge {
        left: "o",
        right: "f",
    },
    BpeMerge {
        left: "i",
        right: "s",
    },
    BpeMerge {
        left: "i",
        right: "t",
    },
    BpeMerge {
        left: "f",
        right: "o",
    },
    BpeMerge {
        left: "fo",
        right: "r",
    },
    BpeMerge {
        left: "t",
        right: "h",
    }, // dup-safe
    BpeMerge {
        left: "w",
        right: "a",
    },
    BpeMerge {
        left: "wa",
        right: "s",
    },
    BpeMerge {
        left: "o",
        right: "n",
    },
    BpeMerge {
        left: "a",
        right: "r",
    },
    BpeMerge {
        left: "ar",
        right: "e",
    },
    BpeMerge {
        left: "w",
        right: "i",
    },
    BpeMerge {
        left: "wi",
        right: "t",
    },
    BpeMerge {
        left: "wit",
        right: "h",
    },
    BpeMerge {
        left: "a",
        right: "s",
    },
    BpeMerge {
        left: "h",
        right: "e",
    },
    BpeMerge {
        left: "b",
        right: "e",
    },
    BpeMerge {
        left: "a",
        right: "t",
    },
    BpeMerge {
        left: "b",
        right: "y",
    },
    BpeMerge {
        left: "t",
        right: "h",
    },
    BpeMerge {
        left: "th",
        right: "i",
    },
    BpeMerge {
        left: "thi",
        right: "s",
    },
    BpeMerge {
        left: "h",
        right: "a",
    },
    BpeMerge {
        left: "ha",
        right: "v",
    },
    BpeMerge {
        left: "hav",
        right: "e",
    },
    BpeMerge {
        left: "f",
        right: "r",
    },
    BpeMerge {
        left: "fr",
        right: "o",
    },
    BpeMerge {
        left: "fro",
        right: "m",
    },
    BpeMerge {
        left: "o",
        right: "r",
    },
    BpeMerge {
        left: "n",
        right: "o",
    },
    BpeMerge {
        left: "no",
        right: "t",
    },
    BpeMerge {
        left: "b",
        right: "u",
    },
    BpeMerge {
        left: "bu",
        right: "t",
    },
    BpeMerge {
        left: "i",
        right: "f",
    },
    BpeMerge {
        left: "w",
        right: "e",
    },
    BpeMerge {
        left: "c",
        right: "a",
    },
    BpeMerge {
        left: "ca",
        right: "n",
    },
    BpeMerge {
        left: "a",
        right: "n",
    },
    BpeMerge {
        left: "w",
        right: "h",
    },
    BpeMerge {
        left: "wh",
        right: "i",
    },
    BpeMerge {
        left: "whi",
        right: "c",
    },
    BpeMerge {
        left: "whic",
        right: "h",
    },
    BpeMerge {
        left: "y",
        right: "o",
    },
    BpeMerge {
        left: "yo",
        right: "u",
    },
    BpeMerge {
        left: "a",
        right: "l",
    },
    BpeMerge {
        left: "al",
        right: "l",
    },
    BpeMerge {
        left: "w",
        right: "i",
    },
    BpeMerge {
        left: "wi",
        right: "l",
    },
    BpeMerge {
        left: "wil",
        right: "l",
    },
    BpeMerge {
        left: "u",
        right: "s",
    },
    BpeMerge {
        left: "us",
        right: "e",
    },
    BpeMerge {
        left: "f",
        right: "i",
    },
    BpeMerge {
        left: "fi",
        right: "l",
    },
    BpeMerge {
        left: "fil",
        right: "e",
    },
    BpeMerge {
        left: "c",
        right: "o",
    },
    BpeMerge {
        left: "co",
        right: "d",
    },
    BpeMerge {
        left: "cod",
        right: "e",
    },
    BpeMerge {
        left: "f",
        right: "u",
    },
    BpeMerge {
        left: "fu",
        right: "n",
    },
    BpeMerge {
        left: "fun",
        right: "c",
    },
    BpeMerge {
        left: "func",
        right: "t",
    },
    BpeMerge {
        left: "funct",
        right: "i",
    },
    BpeMerge {
        left: "functi",
        right: "o",
    },
    BpeMerge {
        left: "functio",
        right: "n",
    },
    // ── Rust keywords ────────────────────────────────────────────────────
    BpeMerge {
        left: "f",
        right: "n",
    },
    BpeMerge {
        left: "l",
        right: "e",
    },
    BpeMerge {
        left: "le",
        right: "t",
    },
    BpeMerge {
        left: "p",
        right: "u",
    },
    BpeMerge {
        left: "pu",
        right: "b",
    },
    BpeMerge {
        left: "m",
        right: "u",
    },
    BpeMerge {
        left: "mu",
        right: "t",
    },
    BpeMerge {
        left: "u",
        right: "s",
    },
    BpeMerge {
        left: "us",
        right: "e",
    },
    BpeMerge {
        left: "m",
        right: "o",
    },
    BpeMerge {
        left: "mo",
        right: "d",
    },
    BpeMerge {
        left: "i",
        right: "m",
    },
    BpeMerge {
        left: "im",
        right: "p",
    },
    BpeMerge {
        left: "imp",
        right: "l",
    },
    BpeMerge {
        left: "impl",
        right: " ",
    },
    BpeMerge {
        left: "s",
        right: "t",
    },
    BpeMerge {
        left: "st",
        right: "r",
    },
    BpeMerge {
        left: "str",
        right: "u",
    },
    BpeMerge {
        left: "stru",
        right: "c",
    },
    BpeMerge {
        left: "struc",
        right: "t",
    },
    BpeMerge {
        left: "struct",
        right: " ",
    },
    BpeMerge {
        left: "e",
        right: "n",
    },
    BpeMerge {
        left: "en",
        right: "u",
    },
    BpeMerge {
        left: "enu",
        right: "m",
    },
    BpeMerge {
        left: "r",
        right: "e",
    },
    BpeMerge {
        left: "re",
        right: "t",
    },
    BpeMerge {
        left: "ret",
        right: "u",
    },
    BpeMerge {
        left: "retu",
        right: "r",
    },
    BpeMerge {
        left: "return",
        right: " ",
    },
    BpeMerge {
        left: "m",
        right: "a",
    },
    BpeMerge {
        left: "ma",
        right: "t",
    },
    BpeMerge {
        left: "mat",
        right: "c",
    },
    BpeMerge {
        left: "matc",
        right: "h",
    },
    BpeMerge {
        left: "match",
        right: " ",
    },
    // ── Space + common word merges ──────────────────────────────────────
    BpeMerge {
        left: " ",
        right: "the",
    },
    BpeMerge {
        left: " ",
        right: "a",
    },
    BpeMerge {
        left: " ",
        right: "and",
    },
    BpeMerge {
        left: " ",
        right: "to",
    },
    BpeMerge {
        left: " ",
        right: "of",
    },
    BpeMerge {
        left: " ",
        right: "in",
    },
    BpeMerge {
        left: " ",
        right: "is",
    },
    BpeMerge {
        left: " ",
        right: "it",
    },
    BpeMerge {
        left: " ",
        right: "for",
    },
    BpeMerge {
        left: " ",
        right: "that",
    },
    BpeMerge {
        left: " ",
        right: "was",
    },
    BpeMerge {
        left: " ",
        right: "on",
    },
    BpeMerge {
        left: " ",
        right: "are",
    },
    BpeMerge {
        left: " ",
        right: "with",
    },
    BpeMerge {
        left: " ",
        right: "as",
    },
    BpeMerge {
        left: " ",
        right: "he",
    },
    BpeMerge {
        left: " ",
        right: "be",
    },
    BpeMerge {
        left: " ",
        right: "at",
    },
    BpeMerge {
        left: " ",
        right: "by",
    },
    BpeMerge {
        left: " ",
        right: "this",
    },
    BpeMerge {
        left: " ",
        right: "have",
    },
    BpeMerge {
        left: " ",
        right: "from",
    },
    BpeMerge {
        left: " ",
        right: "or",
    },
    BpeMerge {
        left: " ",
        right: "not",
    },
    BpeMerge {
        left: " ",
        right: "but",
    },
    BpeMerge {
        left: " ",
        right: "if",
    },
    BpeMerge {
        left: " ",
        right: "we",
    },
    BpeMerge {
        left: " ",
        right: "can",
    },
    BpeMerge {
        left: " ",
        right: "an",
    },
    BpeMerge {
        left: " ",
        right: "which",
    },
    BpeMerge {
        left: " ",
        right: "you",
    },
    BpeMerge {
        left: " ",
        right: "all",
    },
    BpeMerge {
        left: " ",
        right: "will",
    },
    BpeMerge {
        left: " ",
        right: "use",
    },
    BpeMerge {
        left: " ",
        right: "file",
    },
    BpeMerge {
        left: " ",
        right: "code",
    },
    BpeMerge {
        left: " ",
        right: "function",
    },
    // ── Code-specific multi-char merges ─────────────────────────────────
    BpeMerge {
        left: ":",
        right: ":",
    },
    BpeMerge {
        left: "=",
        right: ">",
    },
    BpeMerge {
        left: "(",
        right: ")",
    },
    BpeMerge {
        left: "{",
        right: "}",
    },
    BpeMerge {
        left: " ",
        right: "->",
    },
    BpeMerge {
        left: " ",
        right: "=>",
    },
    BpeMerge {
        left: ":",
        right: "/",
    },
    BpeMerge {
        left: ":/",
        right: "/",
    },
    BpeMerge {
        left: "h",
        right: "t",
    },
    BpeMerge {
        left: "ht",
        right: "t",
    },
    BpeMerge {
        left: "htt",
        right: "p",
    },
    BpeMerge {
        left: "p",
        right: "s",
    },
    BpeMerge {
        left: "http",
        right: "s",
    },
    BpeMerge {
        left: "https",
        right: "://",
    },
    // ── Trigram merges (space+word+space) ───────────────────────────────
    BpeMerge {
        left: " the",
        right: " ",
    },
    BpeMerge {
        left: " and",
        right: " ",
    },
    BpeMerge {
        left: " to",
        right: " ",
    },
    BpeMerge {
        left: " of",
        right: " ",
    },
    BpeMerge {
        left: " in",
        right: " ",
    },
    BpeMerge {
        left: " is",
        right: " ",
    },
    BpeMerge {
        left: " for",
        right: " ",
    },
    BpeMerge {
        left: " with",
        right: " ",
    },
    BpeMerge {
        left: " that",
        right: " ",
    },
    // ── Whitespace compaction ───────────────────────────────────────────
    BpeMerge {
        left: "  ",
        right: " ",
    },
    BpeMerge {
        left: " \n",
        right: "",
    },
    BpeMerge {
        left: "\n",
        right: "\n",
    },
    // ── Punctuation attachment ───────────────────────────────────────────
    BpeMerge {
        left: " ",
        right: ".",
    },
    BpeMerge {
        left: " ",
        right: ",",
    },
    BpeMerge {
        left: " ",
        right: ":",
    },
    BpeMerge {
        left: " ",
        right: ";",
    },
    BpeMerge {
        left: " ",
        right: "!",
    },
    BpeMerge {
        left: " ",
        right: "?",
    },
    BpeMerge {
        left: " ",
        right: "#",
    },
    BpeMerge {
        left: " ",
        right: "-",
    },
    BpeMerge {
        left: " ",
        right: "--",
    },
    BpeMerge {
        left: "/",
        right: "/",
    },
    BpeMerge {
        left: " //",
        right: " ",
    },
    BpeMerge {
        left: " ",
        right: "/",
    },
    BpeMerge {
        left: " ",
        right: "\"",
    },
];

/// Count tokens in `text` using the BPE-like tokenizer.
///
/// Returns at least 1 for any input (including empty string, which is
/// treated as a single padding/separator token).
///
/// # Example
///
/// ```
/// use raven::tokenizer::count_tokens;
/// assert!(count_tokens("hello world") >= 2);
/// assert_eq!(count_tokens(""), 1);
/// assert!(count_tokens("fn main() -> Result<(), anyhow::Error> { }") >= 8);
/// ```
pub fn count_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 1;
    }

    // Step 1: Pre-tokenize using the GPT-style regex
    let pretokens: Vec<&str> = pretoken_re().find_iter(text).map(|m| m.as_str()).collect();

    if pretokens.is_empty() {
        return 1;
    }

    // Step 2: Build a single token list from all pre-tokens (characters),
    // then apply BPE merges across the full sequence. This allows merges
    // like (" ", "the") to work across pre-token boundaries.
    let mut tokens: Vec<String> = Vec::new();
    for pt in &pretokens {
        for c in pt.chars() {
            tokens.push(c.to_string());
        }
    }

    // Step 3: Apply BPE merges iteratively
    let max_iterations = tokens.len() * 2 + 1;
    for _ in 0..max_iterations {
        let mut best_merge: Option<(usize, usize)> = None; // (merge_rank, position)
        for i in 0..tokens.len().saturating_sub(1) {
            let left = tokens[i].as_str();
            let right = tokens[i + 1].as_str();
            for (rank, merge) in MERGES.iter().enumerate() {
                if merge.left == left && merge.right == right {
                    best_merge = Some((rank, i));
                    break;
                }
            }
            if best_merge.is_some() {
                break; // found a merge at this position, restart scan
            }
        }

        match best_merge {
            Some((rank, pos)) => {
                let merged = format!("{}{}", MERGES[rank].left, MERGES[rank].right);
                tokens.splice(pos..=pos + 1, std::iter::once(merged));
            }
            None => break,
        }
    }

    let total = tokens.len();

    // Add ~4% overhead for structural tokens (role markers, separators)
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
        // 8 separate "the" + 8 spaces = 16 raw tokens, thanks to merges
        let repeated = count_tokens("the the the the the the the the");
        // At minimum each "the" + space is 2 tokens, so 16 tokens without merges.
        // With " the" merges, should be significantly less.
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
        // " the" should be 1 merge, so " the" is fewer tokens than " " + "the"
        let merged = count_tokens("the the the");
        let unmerged_approx = 3 + 3; // 3x "the" (3 chars each, ~2 tokens) + 2x " " = ~8
        assert!(
            merged <= unmerged_approx,
            "merges should reduce count: {} vs approx {}",
            merged,
            unmerged_approx
        );
    }
}
