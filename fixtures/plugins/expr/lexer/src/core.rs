//! Lexer core logic shared by the lexer dylib wrapper and higher-level expression plugins.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Number(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Exclamation,
    LParen,
    RParen,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub kind: TokenKind,
    pub position: usize,
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LexError {
    #[error("unexpected token at position {position}")]
    UnexpectedToken { position: usize },
    #[error("invalid number `{text}` at position {position}")]
    InvalidNumber { text: String, position: usize },
}

pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let chars: Vec<char> = src.chars().collect();
    let mut pos = 0;
    let mut out = Vec::new();

    while pos < chars.len() {
        let ch = chars[pos];
        if ch.is_whitespace() {
            pos += 1;
            continue;
        }

        let token = match ch {
            '+' => {
                pos += 1;
                Token {
                    kind: TokenKind::Plus,
                    position: pos - 1,
                }
            }
            '-' => {
                pos += 1;
                Token {
                    kind: TokenKind::Minus,
                    position: pos - 1,
                }
            }
            '*' => {
                pos += 1;
                Token {
                    kind: TokenKind::Star,
                    position: pos - 1,
                }
            }
            '/' => {
                pos += 1;
                Token {
                    kind: TokenKind::Slash,
                    position: pos - 1,
                }
            }
            '%' => {
                pos += 1;
                Token {
                    kind: TokenKind::Percent,
                    position: pos - 1,
                }
            }
            '^' => {
                pos += 1;
                Token {
                    kind: TokenKind::Caret,
                    position: pos - 1,
                }
            }
            '!' => {
                pos += 1;
                Token {
                    kind: TokenKind::Exclamation,
                    position: pos - 1,
                }
            }
            '(' => {
                pos += 1;
                Token {
                    kind: TokenKind::LParen,
                    position: pos - 1,
                }
            }
            ')' => {
                pos += 1;
                Token {
                    kind: TokenKind::RParen,
                    position: pos - 1,
                }
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = pos;
                let mut seen_dot = false;
                while pos < chars.len() {
                    let cur = chars[pos];
                    if cur.is_ascii_digit() {
                        pos += 1;
                        continue;
                    }
                    if cur == '.' && !seen_dot {
                        seen_dot = true;
                        pos += 1;
                        continue;
                    }
                    break;
                }
                let text = chars[start..pos].iter().collect::<String>();
                let value = text.parse::<f64>().map_err(|_| LexError::InvalidNumber {
                    text: text.clone(),
                    position: start,
                })?;
                Token {
                    kind: TokenKind::Number(value),
                    position: start,
                }
            }
            _ => {
                return Err(LexError::UnexpectedToken { position: pos });
            }
        };

        out.push(token);
    }

    Ok(out)
}
