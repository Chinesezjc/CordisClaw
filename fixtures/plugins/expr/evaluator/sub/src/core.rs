//! Subtract core logic shared by the sub dylib wrapper and evaluator parent plugin.

#[derive(Debug, Default, Clone, Copy)]
pub struct SubPlugin;

impl SubPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> f64 {
        lhs - rhs
    }
}

#[allow(dead_code)]
pub fn apply(lhs: f64, rhs: f64) -> f64 {
    SubPlugin.apply(lhs, rhs)
}
