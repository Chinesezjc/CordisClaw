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

#[test]
fn evaluates_modulo() {
    let value = evaluate_expression("10 % 3").expect("must evaluate");
    assert_eq!(value, 1.0);
}

#[test]
fn rejects_modulo_by_zero() {
    let err = evaluate_expression("10 % 0").expect_err("must fail");
    assert_eq!(err, EvaluateExpressionError::ModuloByZero);
}

#[test]
fn evaluates_power() {
    let value = evaluate_expression("2 ^ 3").expect("must evaluate");
    assert_eq!(value, 8.0);
}

#[test]
fn power_right_associative() {
    let value = evaluate_expression("2 ^ 3 ^ 2").expect("must evaluate");
    assert_eq!(value, 512.0); // 2^(3^2) = 2^9 = 512
}

#[test]
fn power_higher_precedence_than_mul() {
    let value = evaluate_expression("2 * 3 ^ 2").expect("must evaluate");
    assert_eq!(value, 18.0); // 2 * (3^2) = 2 * 9 = 18
}
