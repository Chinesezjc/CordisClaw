//! Pow sub-plugin for expression runtime.
//! It exposes one arithmetic operation as a Rust dylib plugin.

mod core;

pub use core::*;

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BinaryOpRequest {
    lhs: f64,
    rhs: f64,
}

#[derive(Debug, Serialize)]
struct BinaryOpResponse {
    value: f64,
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "expr_evaluator_pow",
        "expr/evaluator/pow",
        "0.1.0",
        None,
        vec![node_doc(
            "expr_pow",
            "Raise lhs to the power of rhs.",
            json!({
                "type": "object",
                "required": ["lhs", "rhs"],
                "properties": {
                    "lhs": { "type": "number" },
                    "rhs": { "type": "number" }
                }
            }),
            json!({
                "type": "object",
                "properties": { "value": { "type": "number" } }
            }),
            &[],
            &[],
        )],
    None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_expr_pow_v1", "api_v2")
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    let response = match serde_json::from_str::<BinaryOpRequest>(&req.payload) {
        Ok(request) => BinaryOpResponse {
            value: apply(request.lhs, request.rhs),
        },
        Err(_) => BinaryOpResponse { value: f64::NAN },
    };
    json_response(&response)
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
