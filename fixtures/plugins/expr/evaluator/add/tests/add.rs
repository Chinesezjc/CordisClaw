use expr_evaluator_add::apply;

#[test]
fn add_works() {
    assert_eq!(apply(1.5, 2.0), 3.5);
}
