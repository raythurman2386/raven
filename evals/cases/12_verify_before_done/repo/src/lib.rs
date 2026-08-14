/// Clamp n into [lo, hi].
pub fn clamp(n: i32, lo: i32, hi: i32) -> i32 {
    if n < lo {
        lo
    } else if n > hi {
        lo // BUG: should be hi
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below() {
        assert_eq!(clamp(-1, 0, 10), 0);
    }

    #[test]
    fn above() {
        assert_eq!(clamp(99, 0, 10), 10);
    }

    #[test]
    fn inside() {
        assert_eq!(clamp(5, 0, 10), 5);
    }
}
