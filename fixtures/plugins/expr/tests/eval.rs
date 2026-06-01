use expr::{evaluate_expression, ExprError};

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

#[test]
fn evaluates_modulo() {
    let value = evaluate_expression("10 % 3").expect("must evaluate");
    assert_eq!(value, 1.0);
}

#[test]
fn rejects_modulo_by_zero() {
    let err = evaluate_expression("10 % 0").expect_err("must fail");
    assert_eq!(err, ExprError::ModuloByZero);
}

#[test]
fn evaluates_modulo_with_precedence() {
    let value = evaluate_expression("10 + 7 % 3").expect("must evaluate");
    assert_eq!(value, 11.0);
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

#[test]
fn unary_minus_binds_before_power() {
    // With the current grammar: Factor := ('+'|'-') Factor
    // `-2^2` parses as (-2)^2 = 4, since unary minus is part of Factor
    // which is the left-hand side of Power.
    let value = evaluate_expression("-2 ^ 2").expect("must evaluate");
    assert_eq!(value, 4.0);
}
