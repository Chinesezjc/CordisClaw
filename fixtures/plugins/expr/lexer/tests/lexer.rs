use expr_lexer::{lex, LexError, TokenKind};

#[test]
fn lexes_expression_tokens() {
    let tokens = lex("1 + 2*(3-4)").expect("must lex");
    let kinds = tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            TokenKind::Number(1.0),
            TokenKind::Plus,
            TokenKind::Number(2.0),
            TokenKind::Star,
            TokenKind::LParen,
            TokenKind::Number(3.0),
            TokenKind::Minus,
            TokenKind::Number(4.0),
            TokenKind::RParen,
        ]
    );
}

#[test]
fn rejects_unexpected_character() {
    // `$` is not part of the grammar. (This test used to pass `1 + a` and
    // demand a failure, but `a` is a legal identifier — the lexer was right
    // and the assertion was stale.)
    let err = lex("1 + $").expect_err("must fail");
    assert_eq!(err, LexError::UnexpectedToken { position: 4 });
}

#[test]
fn lexes_identifiers() {
    let tokens = lex("1 + a").expect("identifiers are legal");
    let kinds = tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            TokenKind::Number(1.0),
            TokenKind::Plus,
            TokenKind::Identifier("a".to_string()),
        ]
    );
}

#[test]
fn rejects_invalid_number() {
    let err = lex(".").expect_err("must fail");
    assert_eq!(
        err,
        LexError::InvalidNumber {
            text: ".".to_string(),
            position: 0,
        }
    );
}
