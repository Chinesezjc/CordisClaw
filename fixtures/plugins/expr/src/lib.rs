//! External expression plugin crate.
//! It provides arithmetic expression evaluation for shell command: `Expr <expression>`.

#[path = "../evaluator/src/lib.rs"]
mod evaluator_plugin;

pub use evaluator_plugin::EvaluateExpressionError as ExprError;

/// Evaluate one arithmetic expression.
///
/// Grammar:
/// Expr   := Term (('+'|'-') Term)*
/// Term   := Factor (('*'|'/') Factor)*
/// Factor := Number | '(' Expr ')' | ('+'|'-') Factor
pub fn evaluate_expression(expr: &str) -> Result<f64, ExprError> {
    evaluator_plugin::evaluate_expression(expr)
}
