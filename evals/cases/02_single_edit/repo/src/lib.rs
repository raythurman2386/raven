/// Return twice the input.
pub fn double(n: i32) -> i32 {
    n // BUG: should be n * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_positive() {
        assert_eq!(double(3), 6);
    }

    #[test]
    fn doubles_zero() {
        assert_eq!(double(0), 0);
    }
}
