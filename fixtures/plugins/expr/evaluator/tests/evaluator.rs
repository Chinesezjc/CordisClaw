use expr_evaluator::{evaluate_expression, EvaluateExpressionError};

#[test]
fn evaluates_binary_expression() {
    let value = evaluate_expression("(1 + 2) * 3").expect("must evaluate");
    assert_eq!(value, 9.0);
}

#[test]
fn rejects_division_by_zero() {
    let err = evaluate_expression("1 / 0").expect_err("must fail");
    assert_eq!(err, EvaluateExpressionError::DivisionByZero);
}
