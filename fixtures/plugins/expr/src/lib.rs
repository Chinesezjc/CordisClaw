//! External expression plugin crate.
//! It provides arithmetic expression evaluation for shell command: `Expr <expression>`.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[path = "../evaluator/src/core.rs"]
mod evaluator_core;

pub use evaluator_core::EvaluateExpressionError as ExprError;

#[derive(Debug, Deserialize)]
struct ExprRequest {
    expression: String,
}

#[derive(Debug, Serialize)]
struct ExprResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Evaluate one arithmetic expression.
///
/// Grammar:
/// Expr   := Term (('+'|'-') Term)*
/// Term   := Power (('*'|'/'|'%') Power)*
/// Power  := Factor ('^' Power)?
/// Factor := Number | '(' Expr ')' | ('+'|'-') Factor
pub fn evaluate_expression(expr: &str) -> Result<f64, ExprError> {
    evaluator_core::evaluate_expression(expr)
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "expr",
        "expr",
        "0.1.0",
        Some("Expr"),
        vec![node_doc(
            "expr_entry",
            "Evaluate one arithmetic expression and return a numeric value.",
            json!({
                "type": "object",
                "required": ["expression"],
                "properties": {
                    "expression": { "type": "string" }
                }
            }),
            json!({
                "type": "object",
                "properties": {
                    "value": { "type": "number" },
                    "error": { "type": "string" }
                }
            }),
            &[],
            &["division by zero", "invalid number", "unexpected token"],
        )],
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_expr_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    let response = match serde_json::from_str::<ExprRequest>(&req.payload) {
        Ok(request) => match evaluate_expression(&request.expression) {
            Ok(value) => ExprResponse {
                value: Some(value),
                error: None,
            },
            Err(err) => ExprResponse {
                value: None,
                error: Some(err.to_string()),
            },
        },
        Err(err) => ExprResponse {
            value: None,
            error: Some(format!("invalid request: {err}")),
        },
    };

    json_response(&response)
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
