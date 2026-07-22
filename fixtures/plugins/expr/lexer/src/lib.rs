//! Lexer sub-plugin for expression runtime.
//! It converts source text into a token stream and exports a Rust dylib entrypoint.

mod core;

pub use core::*;

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
struct LexerRequest {
    expression: String,
}

#[derive(Debug, Serialize)]
struct LexerResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<Vec<Token>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "expr_lexer",
        "expr/lexer",
        "0.1.0",
        None,
        vec![node_doc(
            "expr_lexer",
            "Convert expression text into tokens.",
            json!({
                "type": "object",
                "required": ["expression"],
                "properties": { "expression": { "type": "string" } }
            }),
            json!({
                "type": "object",
                "properties": { "tokens": { "type": "array" } }
            }),
            &[],
            &["invalid number", "unexpected token"],
        )],
        None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_expr_lexer_v1", "api_v2")
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    let response = match serde_json::from_str::<LexerRequest>(&req.payload) {
        Ok(request) => match lex(&request.expression) {
            Ok(tokens) => LexerResponse {
                tokens: Some(tokens),
                error: None,
            },
            Err(err) => LexerResponse {
                tokens: None,
                error: Some(err.to_string()),
            },
        },
        Err(err) => LexerResponse {
            tokens: None,
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
