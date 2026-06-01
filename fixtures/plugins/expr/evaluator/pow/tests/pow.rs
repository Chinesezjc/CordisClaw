use expr_evaluator_pow::apply;

#[test]
fn pow_positive_integers() {
    let value = apply(2.0, 3.0);
    assert_eq!(value, 8.0);
}

#[test]
fn pow_zero_exponent() {
    let value = apply(5.0, 0.0);
    assert_eq!(value, 1.0);
}

#[test]
fn pow_negative_exponent() {
    let value = apply(2.0, -1.0);
    assert_eq!(value, 0.5);
}
