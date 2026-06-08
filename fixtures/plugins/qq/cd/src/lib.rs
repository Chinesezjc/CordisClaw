mod core;

pub use core::*;

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
struct CdRequest {
    node_id: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct CdResponse {
    ok: bool,
    node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "qq_cd",
        "qq/cd",
        "0.1.0",
        None,
        vec![
            node_doc(
                "cd_entry",
                "Continuous Deployment child plugin for QQ - handles automated deployment tasks",
                json!({
                    "type": "object",
                    "required": ["node_id"],
                    "properties": {
                        "node_id": {"type": "string", "const": "cd_entry"},
                        "action": {"type": "string", "description": "configure | deploy | status"},
                        "url": {"type": "string", "description": "Deployment target URL"},
                        "api_key": {"type": "string", "description": "API key for deployment"},
                        "project": {"type": "string", "description": "Project name"}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": {"type": "boolean"},
                        "node_id": {"type": "string"},
                        "message": {"type": ["string", "null"]},
                        "data": {},
                        "error": {"type": ["string", "null"]}
                    }
                }),
                &["triggers deployment to configured target"],
                &["not configured", "deploy failed", "invalid action"],
            ),
        ],
        Some("Continuous Deployment plugin for QQ. Use action 'configure' to set up deployment target, 'deploy' to trigger deployment, 'status' to get current configuration."),
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_qq_cd_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn handle(request: &CdRequest) -> Result<CdResponse, String> {
    let plugin = get_instance();
    match request.action.as_deref().unwrap_or("") {
        "configure" => {
            let url = request.url.as_deref().ok_or("url required for configure")?;
            let key = request.api_key.as_deref().ok_or("api_key required for configure")?;
            let project = request.project.as_deref().ok_or("project required for configure")?;
            plugin.configure(url, key, project).map_err(|e| e.to_string())?;
            Ok(CdResponse {
                ok: true,
                node_id: "cd_entry".to_string(),
                message: Some("configured".to_string()),
                data: None,
                error: None,
            })
        }
        "deploy" => {
            let msg = plugin.deploy().map_err(|e| e.to_string())?;
            Ok(CdResponse {
                ok: true,
                node_id: "cd_entry".to_string(),
                message: Some(msg),
                data: None,
                error: None,
            })
        }
        "status" => {
            let cfg = plugin.status();
            Ok(CdResponse {
                ok: true,
                node_id: "cd_entry".to_string(),
                message: Some("status".to_string()),
                data: Some(serde_json::to_value(&cfg).unwrap_or_default()),
                error: None,
            })
        }
        other => Err(format!("unsupported action: {other}")),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<CdRequest>(&req.payload)
        .map_err(|e| format!("cd plugin: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&CdResponse {
            ok: false,
            node_id: "error".to_string(),
            message: None,
            data: None,
            error: Some(e),
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
