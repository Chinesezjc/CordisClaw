use expr_parser::{parse_expression, BinaryOp, ExprAst, ParseExpressionError};

#[test]
fn parses_with_operator_precedence() {
    let ast = parse_expression("1 + 2 * 3").expect("must parse");

    match ast {
        ExprAst::Binary {
            op: BinaryOp::Add,
            lhs,
            rhs,
        } => {
            assert_eq!(*lhs, ExprAst::Number(1.0));
            assert!(matches!(
                *rhs,
                ExprAst::Binary {
                    op: BinaryOp::Mul,
                    ..
                }
            ));
        }
        _ => panic!("unexpected ast shape"),
    }
}

#[test]
fn rejects_missing_right_paren() {
    let err = parse_expression("(1 + 2").expect_err("must fail");
    assert!(matches!(err, ParseExpressionError::MissingRightParen { .. }));
}

#[test]
fn rejects_expected_number() {
    let err = parse_expression("1 + * 2").expect_err("must fail");
    assert_eq!(err, ParseExpressionError::ExpectedNumber { position: 4 });
}
