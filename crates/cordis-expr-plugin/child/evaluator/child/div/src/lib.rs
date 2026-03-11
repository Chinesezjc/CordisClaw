//! Divide sub-plugin for expression runtime.

use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
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

pub fn apply(lhs: f64, rhs: f64) -> Result<f64, DivError> {
    DivPlugin.apply(lhs, rhs)
}
