use cordis_expr_add_plugin::apply;

#[test]
fn add_works() {
    assert_eq!(apply(1.5, 2.0), 3.5);
}
