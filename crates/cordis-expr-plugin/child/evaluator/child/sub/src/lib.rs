//! Subtract sub-plugin for expression runtime.

#[derive(Debug, Default, Clone, Copy)]
pub struct SubPlugin;

impl SubPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> f64 {
        lhs - rhs
    }
}

pub fn apply(lhs: f64, rhs: f64) -> f64 {
    SubPlugin.apply(lhs, rhs)
}
