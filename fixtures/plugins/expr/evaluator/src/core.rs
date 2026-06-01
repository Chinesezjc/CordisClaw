//! Evaluator core logic shared by the evaluator dylib wrapper and the top-level expr plugin.

#[path = "../../parser/src/core.rs"]
pub mod parser_core;
#[path = "../add/src/core.rs"]
pub mod add_core;
#[path = "../sub/src/core.rs"]
pub mod sub_core;
#[path = "../mul/src/core.rs"]
pub mod mul_core;
#[path = "../div/src/core.rs"]
pub mod div_core;
#[path = "../modulo/src/core.rs"]
pub mod modulo_core;
#[path = "../pow/src/core.rs"]
pub mod pow_core;

pub use add_core::AddPlugin;
pub use div_core::{DivError, DivPlugin};
pub use modulo_core::{ModError, ModPlugin};
pub use mul_core::MulPlugin;
pub use pow_core::PowPlugin;
pub use parser_core::{BinaryOp, ExprAst, ParseExpressionError, UnaryOp};
pub use sub_core::SubPlugin;
use thiserror::Error;
use serde::{Deserialize, Serialize};

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalError {
    #[error("division by zero")]
    DivisionByZero,
    #[error("modulo by zero")]
    ModuloByZero,
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluateExpressionError {
    #[error("division by zero")]
    DivisionByZero,
    #[error("modulo by zero")]
    ModuloByZero,
    #[error("unexpected token at position {position}")]
    UnexpectedToken { position: usize },
    #[error("missing ')' at position {position}")]
    MissingRightParen { position: usize },
    #[error("expected number at position {position}")]
    ExpectedNumber { position: usize },
    #[error("invalid number `{text}` at position {position}")]
    InvalidNumber { text: String, position: usize },
}

pub fn evaluate_expression(src: &str) -> Result<f64, EvaluateExpressionError> {
    let ast = parser_core::parse_expression(src).map_err(map_parse_expression_error)?;
    evaluate(&ast).map_err(map_eval_error)
}

pub fn evaluate(ast: &ExprAst) -> Result<f64, EvalError> {
    let ops = OpPlugins::default();
    evaluate_with_plugins(ast, &ops)
}

#[derive(Debug, Default, Clone, Copy)]
struct OpPlugins {
    add: AddPlugin,
    sub: SubPlugin,
    mul: MulPlugin,
    div: DivPlugin,
    modulo: ModPlugin,
    pow: PowPlugin,
}

fn evaluate_with_plugins(ast: &ExprAst, ops: &OpPlugins) -> Result<f64, EvalError> {
    match ast {
        ExprAst::Number(v) => Ok(*v),
        ExprAst::Unary { op, expr } => {
            let value = evaluate_with_plugins(expr, ops)?;
            match op {
                UnaryOp::Plus => Ok(value),
                UnaryOp::Minus => Ok(ops.sub.apply(0.0, value)),
            }
        }
        ExprAst::Binary { op, lhs, rhs } => {
            let left = evaluate_with_plugins(lhs, ops)?;
            let right = evaluate_with_plugins(rhs, ops)?;
            match op {
                BinaryOp::Add => Ok(ops.add.apply(left, right)),
                BinaryOp::Sub => Ok(ops.sub.apply(left, right)),
                BinaryOp::Mul => Ok(ops.mul.apply(left, right)),
                BinaryOp::Div => ops.div.apply(left, right).map_err(|err| match err {
                    DivError::DivisionByZero => EvalError::DivisionByZero,
                }),
                BinaryOp::Mod => ops.modulo.apply(left, right).map_err(|err| match err {
                    ModError::ModuloByZero => EvalError::ModuloByZero,
                }),
                BinaryOp::Pow => Ok(ops.pow.apply(left, right)),
            }
        }
    }
}

fn map_parse_expression_error(err: ParseExpressionError) -> EvaluateExpressionError {
    match err {
        ParseExpressionError::UnexpectedToken { position } => {
            EvaluateExpressionError::UnexpectedToken { position }
        }
        ParseExpressionError::MissingRightParen { position } => {
            EvaluateExpressionError::MissingRightParen { position }
        }
        ParseExpressionError::ExpectedNumber { position } => {
            EvaluateExpressionError::ExpectedNumber { position }
        }
        ParseExpressionError::InvalidNumber { text, position } => {
            EvaluateExpressionError::InvalidNumber { text, position }
        }
    }
}

fn map_eval_error(err: EvalError) -> EvaluateExpressionError {
    match err {
        EvalError::DivisionByZero => EvaluateExpressionError::DivisionByZero,
        EvalError::ModuloByZero => EvaluateExpressionError::ModuloByZero,
    }
}
