//! Add sub-plugin for expression runtime.

#[derive(Debug, Default, Clone, Copy)]
pub struct AddPlugin;

impl AddPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> f64 {
        lhs + rhs
    }
}

pub fn apply(lhs: f64, rhs: f64) -> f64 {
    AddPlugin.apply(lhs, rhs)
}
