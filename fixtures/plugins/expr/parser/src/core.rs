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
    Constant {
        name: String,
        value: f64,
    },
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
    /// P1-45: recursive-descent parser gave the caller a stack overflow
    /// on deeply nested expressions (`(((...)))` or right-recursive
    /// `1^1^1^...`). Now bounded by MAX_PARSE_DEPTH.
    #[error("expression nested too deep (>{limit}) at position {position}")]
    TooDeep { position: usize, limit: usize },
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
    /// P1-45: bubble up TooDeep to the public error shape.
    #[error("expression nested too deep (>{limit}) at position {position}")]
    TooDeep { position: usize, limit: usize },
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
        ParseError::TooDeep { position, limit } => ParseExpressionError::TooDeep { position, limit },
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    /// P1-45: current recursion depth of the parse_expr / parse_term /
    /// parse_power / parse_factor tower. Every entry checks against
    /// `MAX_PARSE_DEPTH` and returns `TooDeep` before recursing.
    depth: usize,
}

/// P1-45: 512 covers any realistic user expression while leaving plenty of
/// stack headroom on the default 8 MiB Rust stack (~16 KiB per parser frame
/// pessimistically = 8 MiB).
const MAX_PARSE_DEPTH: usize = 512;

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            pos: 0,
            depth: 0,
        }
    }

    #[inline]
    fn enter_scope(&mut self) -> Result<(), ParseError> {
        if self.depth >= MAX_PARSE_DEPTH {
            return Err(ParseError::TooDeep {
                position: self.current_position(),
                limit: MAX_PARSE_DEPTH,
            });
        }
        self.depth += 1;
        Ok(())
    }

    #[inline]
    fn exit_scope(&mut self) {
        self.depth = self.depth.saturating_sub(1);
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
        self.enter_scope()?;
        let result = self.parse_expr_body();
        self.exit_scope();
        result
    }

    fn parse_expr_body(&mut self) -> Result<ExprAst, ParseError> {
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
        self.enter_scope()?;
        let result = self.parse_term_body();
        self.exit_scope();
        result
    }

    fn parse_term_body(&mut self) -> Result<ExprAst, ParseError> {
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
        self.enter_scope()?;
        let result = self.parse_power_body();
        self.exit_scope();
        result
    }

    fn parse_power_body(&mut self) -> Result<ExprAst, ParseError> {
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
        self.enter_scope()?;
        let result = self.parse_factor_body();
        self.exit_scope();
        result
    }

    fn parse_factor_body(&mut self) -> Result<ExprAst, ParseError> {
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
            TokenKind::Identifier(name) => {
                self.bump();
                match name.as_str() {
                    "pi" => ExprAst::Constant {
                        name: name.clone(),
                        value: std::f64::consts::PI,
                    },
                    "e" => ExprAst::Constant {
                        name: name.clone(),
                        value: std::f64::consts::E,
                    },
                    _ => {
                        return Err(ParseError::UnexpectedToken {
                            position: token.position,
                        })
                    }
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

// P1-45 DepthGuard removed; parser now uses paired
// enter_scope/exit_scope helper calls at the top/bottom of each
// parse_* method, avoiding the &mut aliasing problem an RAII guard
// would introduce inside the recursive-descent tower.
