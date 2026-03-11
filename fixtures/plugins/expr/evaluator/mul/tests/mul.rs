use expr_evaluator_mul::apply;

#[test]
fn mul_works() {
    assert_eq!(apply(2.5, 4.0), 10.0);
}
