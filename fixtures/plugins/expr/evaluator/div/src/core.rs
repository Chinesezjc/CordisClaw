//! Divide core logic shared by the div dylib wrapper and evaluator parent plugin.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivError {
    #[error("division by zero")]
    DivisionByZero,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DivPlugin;

impl DivPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> Result<f64, DivError> {
        if rhs == 0.0 {
            return Err(DivError::DivisionByZero);
        }
        Ok(lhs / rhs)
    }
}

#[allow(dead_code)]
pub fn apply(lhs: f64, rhs: f64) -> Result<f64, DivError> {
    DivPlugin.apply(lhs, rhs)
}
