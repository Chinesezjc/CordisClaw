use cordis_expr_lexer_plugin::lex;
use cordis_expr_parser_plugin::{parse, BinaryOp, ExprAst, ParseError};

#[test]
fn parses_with_operator_precedence() {
    let tokens = lex("1 + 2 * 3").expect("must lex");
    let ast = parse(&tokens).expect("must parse");

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
    let tokens = lex("(1 + 2").expect("must lex");
    let err = parse(&tokens).expect_err("must fail");
    assert!(matches!(err, ParseError::MissingRightParen { .. }));
}

#[test]
fn rejects_expected_number() {
    let tokens = lex("1 + * 2").expect("must lex");
    let err = parse(&tokens).expect_err("must fail");
    assert_eq!(err, ParseError::ExpectedNumber { position: 4 });
}
