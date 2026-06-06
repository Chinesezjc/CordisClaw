//! Parser core logic shared by the parser dylib wrapper and higher-level expression plugins.

#[path = "../../lexer/src/core.rs"]
pub mod lexer_core;

pub use lexer_core::{LexError, Token, TokenKind};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnaryOp {
    Plus,
    Minus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExprAst {
    Number(f64),
    Unary {
        op: UnaryOp,
        expr: Box<ExprAst>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<ExprAst>,
        rhs: Box<ExprAst>,
    },
    Factorial {
        expr: Box<ExprAst>,
    },
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseError {
    #[error("unexpected token at position {position}")]
    UnexpectedToken { position: usize },
    #[error("missing ')' at position {position}")]
    MissingRightParen { position: usize },
    #[error("expected number at position {position}")]
    ExpectedNumber { position: usize },
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseExpressionError {
    #[error("unexpected token at position {position}")]
    UnexpectedToken { position: usize },
    #[error("missing ')' at position {position}")]
    MissingRightParen { position: usize },
    #[error("expected number at position {position}")]
    ExpectedNumber { position: usize },
    #[error("invalid number `{text}` at position {position}")]
    InvalidNumber { text: String, position: usize },
}

pub fn parse_expression(src: &str) -> Result<ExprAst, ParseExpressionError> {
    let tokens = lexer_core::lex(src).map_err(map_lex_error)?;
    parse(&tokens).map_err(map_parse_error)
}

pub fn parse(tokens: &[Token]) -> Result<ExprAst, ParseError> {
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_expr()?;
    if let Some(token) = parser.peek() {
        return Err(ParseError::UnexpectedToken {
            position: token.position,
        });
    }
    Ok(ast)
}

fn map_lex_error(err: LexError) -> ParseExpressionError {
    match err {
        LexError::UnexpectedToken { position } => ParseExpressionError::UnexpectedToken { position },
        LexError::InvalidNumber { text, position } => {
            ParseExpressionError::InvalidNumber { text, position }
        }
    }
}

fn map_parse_error(err: ParseError) -> ParseExpressionError {
    match err {
        ParseError::UnexpectedToken { position } => ParseExpressionError::UnexpectedToken { position },
        ParseError::MissingRightParen { position } => {
            ParseExpressionError::MissingRightParen { position }
        }
        ParseError::ExpectedNumber { position } => ParseExpressionError::ExpectedNumber { position },
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<&'a Token> {
        let token = self.peek()?;
        self.pos += 1;
        Some(token)
    }

    fn current_position(&self) -> usize {
        self.peek()
            .map(|t| t.position)
            .or_else(|| self.tokens.last().map(|t| t.position + 1))
            .unwrap_or(0)
    }

    fn parse_expr(&mut self) -> Result<ExprAst, ParseError> {
        let mut lhs = self.parse_term()?;
        loop {
            let op = match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Plus) => BinaryOp::Add,
                Some(TokenKind::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_term()?;
            lhs = ExprAst::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_term(&mut self) -> Result<ExprAst, ParseError> {
        let mut lhs = self.parse_power()?;
        loop {
            let op = match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Star) => BinaryOp::Mul,
                Some(TokenKind::Slash) => BinaryOp::Div,
                Some(TokenKind::Percent) => BinaryOp::Mod,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_power()?;
            lhs = ExprAst::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_power(&mut self) -> Result<ExprAst, ParseError> {
        let lhs = self.parse_factor()?;
        if let Some(TokenKind::Caret) = self.peek().map(|t| &t.kind) {
            self.bump();
            let rhs = self.parse_power()?;
            Ok(ExprAst::Binary {
                op: BinaryOp::Pow,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        } else {
            Ok(lhs)
        }
    }

    fn parse_factor(&mut self) -> Result<ExprAst, ParseError> {
        let Some(token) = self.peek() else {
            return Err(ParseError::ExpectedNumber {
                position: self.current_position(),
            });
        };

        let mut expr = match &token.kind {
            TokenKind::Number(value) => {
                self.bump();
                ExprAst::Number(*value)
            }
            TokenKind::Plus => {
                self.bump();
                let inner = self.parse_factor()?;
                ExprAst::Unary {
                    op: UnaryOp::Plus,
                    expr: Box::new(inner),
                }
            }
            TokenKind::Minus => {
                self.bump();
                let inner = self.parse_factor()?;
                ExprAst::Unary {
                    op: UnaryOp::Minus,
                    expr: Box::new(inner),
                }
            }
            TokenKind::LParen => {
                self.bump();
                let inner = self.parse_expr()?;
                match self.bump() {
                    Some(Token {
                        kind: TokenKind::RParen,
                        ..
                    }) => inner,
                    _ => {
                        return Err(ParseError::MissingRightParen {
                            position: self.current_position(),
                        })
                    }
                }
            }
            _ => {
                return Err(ParseError::ExpectedNumber {
                    position: token.position,
                })
            }
        };

        // Postfix factorial: binds tighter than any other operator.
        while let Some(TokenKind::Exclamation) = self.peek().map(|t| &t.kind) {
            self.bump();
            expr = ExprAst::Factorial {
                expr: Box::new(expr),
            };
        }

        Ok(expr)
    }
}
