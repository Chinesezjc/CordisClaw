//! Add core logic shared by the add dylib wrapper and evaluator parent plugin.

#[derive(Debug, Default, Clone, Copy)]
pub struct AddPlugin;

impl AddPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> f64 {
        lhs + rhs
    }
}

#[allow(dead_code)]
pub fn apply(lhs: f64, rhs: f64) -> f64 {
    AddPlugin.apply(lhs, rhs)
}
