//! Factorial core logic shared by the factorial dylib wrapper and evaluator parent plugin.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactorialError {
    #[error("factorial requires a non-negative integer")]
    FactorialDomainError,
    /// P1-46: cap the input at 170. f64 can only represent 170! exactly-ish
    /// (≈ 7.257e306); 171! overflows to +inf. Accepting values beyond that
    /// used to iterate for CPU-forever without producing a useful answer
    /// — e.g. `factorial(10000000000)` = 10¹⁰ loop iterations of wasted
    /// work. Reject up front.
    #[error("factorial argument exceeds representable range (max 170)")]
    FactorialOverflow,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FactorialPlugin;

impl FactorialPlugin {
    pub fn apply(&self, n: f64) -> Result<f64, FactorialError> {
        if n < 0.0 {
            return Err(FactorialError::FactorialDomainError);
        }
        if n.fract() != 0.0 {
            return Err(FactorialError::FactorialDomainError);
        }
        if n > 170.0 {
            return Err(FactorialError::FactorialOverflow);
        }
        let n = n as u64;
        if n <= 1 {
            return Ok(1.0);
        }
        let mut result = 1.0f64;
        for i in 2..=n {
            result *= i as f64;
        }
        Ok(result)
    }
}

#[allow(dead_code)]
pub fn apply(n: f64) -> Result<f64, FactorialError> {
    FactorialPlugin.apply(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1-46: `n > 170` must return `FactorialOverflow` immediately —
    /// otherwise `10000000000!` would spin for 10¹⁰ iterations before
    /// even overflowing. Also protects the callers from a NaN/Inf that
    /// serde_json can't serialise.
    #[test]
    fn factorial_over_170_is_rejected() {
        let err = FactorialPlugin.apply(200.0).unwrap_err();
        assert!(matches!(err, FactorialError::FactorialOverflow));
    }

    #[test]
    fn factorial_at_170_still_finite() {
        let v = FactorialPlugin.apply(170.0).unwrap();
        assert!(v.is_finite(), "170! must fit in f64 (got {v})");
    }

    #[test]
    fn factorial_over_170_avoids_dos_no_infinite_loop() {
        // The point of the cap: reject FAST. If this hangs, the guard
        // regressed.
        let start = std::time::Instant::now();
        let err = FactorialPlugin.apply(1_000_000_000.0).unwrap_err();
        assert!(matches!(err, FactorialError::FactorialOverflow));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "factorial(1e9) must reject fast, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn factorial_domain_errors_still_apply() {
        assert!(matches!(
            FactorialPlugin.apply(-1.0),
            Err(FactorialError::FactorialDomainError)
        ));
        assert!(matches!(
            FactorialPlugin.apply(3.5),
            Err(FactorialError::FactorialDomainError)
        ));
    }

    #[test]
    fn factorial_zero_and_one_return_one() {
        assert_eq!(FactorialPlugin.apply(0.0).unwrap(), 1.0);
        assert_eq!(FactorialPlugin.apply(1.0).unwrap(), 1.0);
    }
}
