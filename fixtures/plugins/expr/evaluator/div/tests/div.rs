use expr_evaluator_div::{apply, DivError};

#[test]
fn div_works() {
    assert_eq!(apply(9.0, 3.0).expect("must divide"), 3.0);
}

#[test]
fn div_rejects_zero() {
    let err = apply(1.0, 0.0).expect_err("must fail");
    assert_eq!(err, DivError::DivisionByZero);
}
