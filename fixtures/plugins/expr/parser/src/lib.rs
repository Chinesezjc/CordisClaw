//! Parser sub-plugin for expression runtime.
//! It builds an AST from lexer tokens and exports a Rust dylib entrypoint.

mod core;

pub use core::*;

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
struct ParserRequest {
    tokens: Vec<Token>,
}

#[derive(Debug, Serialize)]
struct ParserResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    ast: Option<ExprAst>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "expr_parser",
        "expr/parser",
        "0.1.0",
        None,
        vec![node_doc(
            "expr_parser",
            "Build an AST from lexer tokens.",
            json!({
                "type": "object",
                "properties": { "tokens": { "type": "array" } }
            }),
            json!({
                "type": "object",
                "properties": { "ast": { "type": "object" } }
            }),
            &[],
            &["missing right paren", "unexpected token"],
        )],
    None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_expr_parser_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    let response = match serde_json::from_str::<ParserRequest>(&req.payload) {
        Ok(request) => match parse(&request.tokens) {
            Ok(ast) => ParserResponse {
                ast: Some(ast),
                error: None,
            },
            Err(err) => ParserResponse {
                ast: None,
                error: Some(err.to_string()),
            },
        },
        Err(err) => ParserResponse {
            ast: None,
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
