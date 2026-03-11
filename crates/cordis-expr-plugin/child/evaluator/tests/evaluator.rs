use cordis_expr_evaluator_plugin::{evaluate, EvalError};
use cordis_expr_parser_plugin::{BinaryOp, ExprAst};

#[test]
fn evaluates_binary_expression() {
    let ast = ExprAst::Binary {
        op: BinaryOp::Mul,
        lhs: Box::new(ExprAst::Binary {
            op: BinaryOp::Add,
            lhs: Box::new(ExprAst::Number(1.0)),
            rhs: Box::new(ExprAst::Number(2.0)),
        }),
        rhs: Box::new(ExprAst::Number(3.0)),
    };

    let value = evaluate(&ast).expect("must evaluate");
    assert_eq!(value, 9.0);
}

#[test]
fn rejects_division_by_zero() {
    let ast = ExprAst::Binary {
        op: BinaryOp::Div,
        lhs: Box::new(ExprAst::Number(1.0)),
        rhs: Box::new(ExprAst::Number(0.0)),
    };

    let err = evaluate(&ast).expect_err("must fail");
    assert_eq!(err, EvalError::DivisionByZero);
}
