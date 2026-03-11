//! Evaluator sub-plugin for expression runtime.
//! It computes a numeric value from parser AST.

#[path = "../../parser/src/lib.rs"]
mod parser_plugin;
#[path = "../add/src/lib.rs"]
mod add_plugin;
#[path = "../sub/src/lib.rs"]
mod sub_plugin;
#[path = "../mul/src/lib.rs"]
mod mul_plugin;
#[path = "../div/src/lib.rs"]
mod div_plugin;

use add_plugin::AddPlugin;
use div_plugin::{DivError, DivPlugin};
use mul_plugin::MulPlugin;
use parser_plugin::{
    parse_expression,
    BinaryOp,
    ExprAst,
    ParseExpressionError,
    UnaryOp,
};
use sub_plugin::SubPlugin;
use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum EvalError {
    #[error("division by zero")]
    DivisionByZero,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EvaluateExpressionError {
    #[error("division by zero")]
    DivisionByZero,
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
    let ast = parse_expression(src).map_err(map_parse_expression_error)?;
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
    }
}
