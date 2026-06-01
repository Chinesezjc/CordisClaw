use expr_evaluator_modulo::{apply, ModError};

#[test]
fn modulo_works() {
    assert_eq!(apply(10.0, 3.0).expect("must modulo"), 1.0);
}

#[test]
fn modulo_rejects_zero() {
    let err = apply(1.0, 0.0).expect_err("must fail");
    assert_eq!(err, ModError::ModuloByZero);
}
