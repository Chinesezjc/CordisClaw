//! Factorial sub-plugin for expression runtime.
//! It exposes the factorial operation as a Rust dylib plugin.

mod core;

pub use core::*;

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
struct UnaryOpRequest {
    n: f64,
}

#[derive(Debug, Serialize)]
struct UnaryOpResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "expr_evaluator_factorial",
        "expr/evaluator/factorial",
        "0.1.0",
        None,
        vec![node_doc(
            "expr_factorial",
            "Compute the factorial of a non-negative integer n.",
            json!({
                "type": "object",
                "required": ["n"],
                "properties": {
                    "n": { "type": "number" }
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
            &["factorial requires a non-negative integer"],
        )],
    None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_expr_factorial_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    let response = match serde_json::from_str::<UnaryOpRequest>(&req.payload) {
        Ok(request) => match apply(request.n) {
            Ok(value) => UnaryOpResponse {
                value: Some(value),
                error: None,
            },
            Err(err) => UnaryOpResponse {
                value: None,
                error: Some(err.to_string()),
            },
        },
        Err(err) => UnaryOpResponse {
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
