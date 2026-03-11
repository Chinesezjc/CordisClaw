//! Multiply sub-plugin for expression runtime.

#[derive(Debug, Default, Clone, Copy)]
pub struct MulPlugin;

impl MulPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> f64 {
        lhs * rhs
    }
}

#[allow(dead_code)]
pub fn apply(lhs: f64, rhs: f64) -> f64 {
    MulPlugin.apply(lhs, rhs)
}
