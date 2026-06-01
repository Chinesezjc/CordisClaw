//! Pow core logic shared by the pow dylib wrapper and evaluator parent plugin.

#[derive(Debug, Default, Clone, Copy)]
pub struct PowPlugin;

impl PowPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> f64 {
        lhs.powf(rhs)
    }
}

#[allow(dead_code)]
pub fn apply(lhs: f64, rhs: f64) -> f64 {
    PowPlugin.apply(lhs, rhs)
}
