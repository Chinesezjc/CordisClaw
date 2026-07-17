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
