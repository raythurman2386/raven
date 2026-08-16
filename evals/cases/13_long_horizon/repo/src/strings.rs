/// Reverse the order of words in a string, preserving the words themselves.
pub fn reverse_words(s: &str) -> String {
    s.split_whitespace().rev().collect::<Vec<_>>().join(" ")
}

/// Convert a phrase to snake_case (lowercase, words joined by underscores).
pub fn to_snake_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| w.to_uppercase()) // BUG: should be to_lowercase()
        .collect::<Vec<_>>()
        .join("_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_words_basic() {
        assert_eq!(reverse_words("one two three"), "three two one");
    }

    #[test]
    fn snake_case_basic() {
        assert_eq!(to_snake_case("Hello World Foo"), "hello_world_foo");
    }
}
