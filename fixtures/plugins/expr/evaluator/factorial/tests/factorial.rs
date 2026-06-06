use expr_evaluator_factorial::apply;

#[test]
fn factorial_of_zero() {
    let value = apply(0.0).expect("must succeed");
    assert_eq!(value, 1.0);
}

#[test]
fn factorial_of_one() {
    let value = apply(1.0).expect("must succeed");
    assert_eq!(value, 1.0);
}

#[test]
fn factorial_of_five() {
    let value = apply(5.0).expect("must succeed");
    assert_eq!(value, 120.0);
}

#[test]
fn factorial_of_ten() {
    let value = apply(10.0).expect("must succeed");
    assert_eq!(value, 3628800.0);
}

#[test]
fn factorial_of_negative_is_error() {
    let err = apply(-1.0).expect_err("must fail");
    assert!(matches!(
        err,
        expr_evaluator_factorial::FactorialError::FactorialDomainError
    ));
}

#[test]
fn factorial_of_non_integer_is_error() {
    let err = apply(3.5).expect_err("must fail");
    assert!(matches!(
        err,
        expr_evaluator_factorial::FactorialError::FactorialDomainError
    ));
}
