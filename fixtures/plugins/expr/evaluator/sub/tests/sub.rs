use expr_evaluator_sub::apply;

#[test]
fn sub_works() {
    assert_eq!(apply(10.0, 3.25), 6.75);
}
