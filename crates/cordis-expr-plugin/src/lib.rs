//! External expression plugin crate.
//! It provides arithmetic expression evaluation for shell command: `Expr <expression>`.

pub use cordis_expr_evaluator_plugin::EvaluateExpressionError as ExprError;

/// Evaluate one arithmetic expression.
///
/// Grammar:
/// Expr   := Term (('+'|'-') Term)*
/// Term   := Factor (('*'|'/') Factor)*
/// Factor := Number | '(' Expr ')' | ('+'|'-') Factor
pub fn evaluate_expression(expr: &str) -> Result<f64, ExprError> {
    cordis_expr_evaluator_plugin::evaluate_expression(expr)
}
