//! Modulo core logic shared by the modulo dylib wrapper and evaluator parent plugin.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModError {
    #[error("modulo by zero")]
    ModuloByZero,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ModPlugin;

impl ModPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> Result<f64, ModError> {
        if rhs == 0.0 {
            return Err(ModError::ModuloByZero);
        }
        Ok(lhs % rhs)
    }
}

#[allow(dead_code)]
pub fn apply(lhs: f64, rhs: f64) -> Result<f64, ModError> {
    ModPlugin.apply(lhs, rhs)
}
