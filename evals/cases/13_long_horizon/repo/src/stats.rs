/// Mean of a non-empty slice.
pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let sum: f64 = xs.iter().sum();
    sum // BUG: missing division by xs.len()
}

/// Median of a slice (odd length: middle; even: average of the two middles).
pub fn median(xs: &[f64]) -> f64 {
    let mut v: Vec<f64> = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    let mid = n / 2;
    if n % 2 == 1 {
        v[mid]
    } else {
        (v[mid] + v[mid]) / 2.0 // BUG: should be (v[mid-1] + v[mid]) / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_basic() {
        assert!((mean(&[1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < 1e-9);
    }

    #[test]
    fn median_odd() {
        assert!((median(&[5.0, 1.0, 3.0]) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn median_even() {
        assert!((median(&[1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < 1e-9);
    }
}
