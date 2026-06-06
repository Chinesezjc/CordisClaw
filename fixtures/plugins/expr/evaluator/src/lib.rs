//! Evaluator sub-plugin for expression runtime.
//! It computes a numeric value from parser AST and exports a Rust dylib entrypoint.

mod core;

pub use core::*;

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
struct EvaluatorRequest {
    ast: ExprAst,
}

#[derive(Debug, Serialize)]
struct EvaluatorResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "expr_evaluator",
        "expr/evaluator",
        "0.1.0",
        None,
        vec![node_doc(
            "expr_eval",
            "Evaluate the AST and delegate arithmetic to child operator plugins.",
            json!({
                "type": "object",
                "properties": { "ast": { "type": "object" } }
            }),
            json!({
                "type": "object",
                "properties": { "value": { "type": "number" } }
            }),
            &[],
            &["division by zero"],
        )],
    None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_expr_evaluator_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    let response = match serde_json::from_str::<EvaluatorRequest>(&req.payload) {
        Ok(request) => match evaluate(&request.ast) {
            Ok(value) => EvaluatorResponse {
                value: Some(value),
                error: None,
            },
            Err(err) => EvaluatorResponse {
                value: None,
                error: Some(err.to_string()),
            },
        },
        Err(err) => EvaluatorResponse {
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
