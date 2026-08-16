/// Simple monthly payment: P * r / (1 - (1+r)^-n).
pub fn monthly_payment(principal: f64, annual_rate: f64, months: u32) -> f64 {
    if months == 0 {
        return 0.0;
    }
    let r = annual_rate / 12.0;
    if r == 0.0 {
        return principal / months as f64;
    }
    let denom = 1.0 - (1.0 + r).powi(-(months as i32));
    principal * denom // BUG: should be principal * r / denom
}

/// Total after `years` of annually-compounded interest at `rate`.
pub fn interest_after_years(principal: f64, rate: f64, years: u32) -> f64 {
    principal * (1.0 + rate).powi(years as i32) - principal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monthly_payment_zero_rate() {
        // $1000 over 10 months at 0% = $100/mo.
        assert!((monthly_payment(1000.0, 0.0, 10) - 100.0).abs() < 1e-6);
    }

    #[test]
    fn monthly_payment_nonzero_rate() {
        // $12000 at 12% annual (1%/mo) over 12 months ≈ $1066.18.
        let p = monthly_payment(12000.0, 0.12, 12);
        assert!((p - 1066.18).abs() < 1.0, "payment too far off: {p}");
    }

    #[test]
    fn interest_after_years_compounds() {
        // $100 at 10% for 2 years = $121 total, so $21 interest.
        assert!((interest_after_years(100.0, 0.10, 2) - 21.0).abs() < 1e-6);
    }
}
