pub mod stats;
pub mod strings;
pub mod finance;

pub use stats::{mean, median};
pub use strings::{reverse_words, to_snake_case};
pub use finance::{monthly_payment, interest_after_years};
