use cordis_expr_plugin::{evaluate_expression, ExprError};

#[test]
fn evaluates_basic_expression() {
    let value = evaluate_expression("1 + 2 * 3").expect("must evaluate");
    assert_eq!(value, 7.0);
}

#[test]
fn evaluates_parentheses() {
    let value = evaluate_expression("(1 + 2) * (3 + 4)").expect("must evaluate");
    assert_eq!(value, 21.0);
}

#[test]
fn rejects_invalid_expression() {
    let err = evaluate_expression("1 + * 2").expect_err("must fail");
    assert!(matches!(err, ExprError::ExpectedNumber { .. }));
}

#[test]
fn rejects_division_by_zero() {
    let err = evaluate_expression("1 / 0").expect_err("must fail");
    assert_eq!(err, ExprError::DivisionByZero);
}
