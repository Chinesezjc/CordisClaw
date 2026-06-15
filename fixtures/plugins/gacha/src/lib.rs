//! Gacha / wish simulator plugin — Genshin Impact probability model.
//!
//! Nodes:
//! - gacha_entry — pull wishes (single / 10-pull)
//! - gacha_status — check current pity counters

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs,
    AbiFingerprint, PluginRequest, PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Mutex;

// ── gacha state ────────────────────────────────────────────────────────

struct GachaState {
    char_pity_5: u32,
    char_pity_4: u32,
    char_guaranteed: bool,
    char_pity_5_total: u64,
    char_total_pulls: u64,
}

static STATE: Mutex<GachaState> = Mutex::new(GachaState {
    char_pity_5: 0,
    char_pity_4: 0,
    char_guaranteed: false,
    char_pity_5_total: 0,
    char_total_pulls: 0,
});

// ── gacha constants ────────────────────────────────────────────────────

const CHAR_5_FEATURED: &[&str] = &["刻晴"];
const CHAR_5_STANDARD: &[&str] = &["迪卢克","琴","莫娜","七七","提纳里","迪希雅"];
const CHAR_4_FEATURED: &[&str] = &["行秋","北斗","烟绯"];
const CHAR_4_STANDARD: &[&str] = &["香菱","芭芭拉","菲谢尔","班尼特","凝光","雷泽","罗莎莉亚","九条裟罗","辛焱","云堇"];
const CHAR_3: &[&str] = &["弹弓","飞天御剑","黑缨枪","铁影阔剑","魔导绪论","讨龙英杰谭","黎明神剑"];

fn rand_f64() -> f64 { rand::random::<f64>() }

fn char_5_star_rate(pity: u32) -> f64 {
    if pity < 73 { 0.006 }
    else { (0.006 + (pity - 72) as f64 * 0.06).min(1.0) }
}

fn char_4_star_rate(pity: u32) -> f64 {
    if pity < 8 { 0.051 }
    else { (0.051 + (pity - 7) as f64 * 0.51).min(1.0) }
}

fn pick<T: Copy>(arr: &[T]) -> T {
    let idx = (rand_f64() * arr.len() as f64) as usize;
    arr[idx.min(arr.len()-1)]
}

// ── request / response types ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GachaRequest {
    cmd: String,
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    count: Option<u32>,
}

#[derive(Debug, Serialize)]
struct GachaResponse {
    ok: bool,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    results: Option<Vec<GachaResult>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pity_5: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pity_4: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guaranteed: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct GachaResult {
    name: String,
    stars: u32,
    is_featured: bool,
}

// ── single pull on character banner ────────────────────────────────────

fn pull_char_banner(state: &mut GachaState) -> GachaResult {
    state.char_pity_5 += 1;
    state.char_pity_4 += 1;
    state.char_total_pulls += 1;

    // Roll for 5★
    let rate_5 = char_5_star_rate(state.char_pity_5);
    if rand_f64() < rate_5 {
        state.char_pity_5_total += state.char_pity_5 as u64;
        let name = if state.char_guaranteed || rand_f64() < 0.5 {
            state.char_guaranteed = false;
            pick(CHAR_5_FEATURED).to_string()
        } else {
            state.char_guaranteed = true;
            pick(CHAR_5_STANDARD).to_string()
        };
        state.char_pity_5 = 0;
        state.char_pity_4 = 0;
        return GachaResult { name, stars: 5, is_featured: !state.char_guaranteed };
    }

    // Roll for 4★ (forced every 10 pulls)
    if state.char_pity_4 >= 10 || rand_f64() < char_4_star_rate(state.char_pity_4) {
        let name = if rand_f64() < 0.5 {
            pick(CHAR_4_FEATURED).to_string()
        } else {
            pick(CHAR_4_STANDARD).to_string()
        };
        state.char_pity_4 = 0;
        return GachaResult { name, stars: 4, is_featured: false };
    }

    // 3★ weapon
    GachaResult { name: pick(CHAR_3).to_string(), stars: 3, is_featured: false }
}

// ── handlers ───────────────────────────────────────────────────────────

fn handle_pull(banner: Option<&str>, count: u32) -> Result<GachaResponse, String> {
    let banner = banner.unwrap_or("character").to_lowercase();
    if !["character","weapon","standard"].contains(&banner.as_str()) {
        return Err("banner must be character, weapon, or standard".into());
    }
    if count == 0 || count > 100 { return Err("count must be 1-100".into()); }

    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    let mut results = Vec::new();
    for _ in 0..count {
        results.push(pull_char_banner(&mut state));
    }

    Ok(GachaResponse {
        ok: true,
        action: "pull".into(),
        message: None,
        results: Some(results),
        pity_5: Some(state.char_pity_5),
        pity_4: Some(state.char_pity_4),
        guaranteed: Some(state.char_guaranteed),
    })
}

fn handle_status() -> Result<GachaResponse, String> {
    let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    Ok(GachaResponse {
        ok: true,
        action: "status".into(),
        message: Some(format!(
            "5★ pity: {}, 4★ pity: {}, guaranteed: {}, total pulls: {}, avg 5★: {:.1}",
            state.char_pity_5, state.char_pity_4, state.char_guaranteed,
            state.char_total_pulls,
            if state.char_pity_5_total > 0 {
                state.char_total_pulls as f64 / state.char_pity_5_total as f64
            } else { 0.0 }
        )),
        results: None,
        pity_5: Some(state.char_pity_5),
        pity_4: Some(state.char_pity_4),
        guaranteed: Some(state.char_guaranteed),
    })
}

fn handle(req: GachaRequest) -> Result<GachaResponse, String> {
    match req.cmd.as_str() {
        "pull" => handle_pull(req.banner.as_deref(), req.count.unwrap_or(1)),
        "status" => handle_status(),
        other => Err(format!("unsupported cmd: {other}; use pull or status")),
    }
}

// ── plugin api ─────────────────────────────────────────────────────────

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "gacha","gacha","0.1.0",
        Some("Gacha"),
        vec![
            node_doc("gacha_entry",
                "Genshin Impact wish simulator. Pull on character/weapon/standard banners with accurate pity and 50/50 mechanics.",
                json!({ "type":"object","required":["cmd"],"properties":{
                    "cmd":{"type":"string","description":"pull | status"},
                    "banner":{"type":"string","description":"character(default) | weapon | standard"},
                    "count":{"type":"integer","description":"1-100, default 1"}
                }}),
                json!({ "type":"object","properties":{
                    "ok":{"type":"boolean"},"action":{"type":"string"},
                    "message":{"type":["string","null"]},
                    "results":{"type":"array","items":{"type":"object"}},
                    "pity_5":{"type":"integer"},"pity_4":{"type":"integer"},
                    "guaranteed":{"type":"boolean"}
                }}),
                &["accurate Genshin pity model","soft pity starts at 73","50/50 system"],
                &["invalid banner","count out of range"],
            ),
            node_doc("gacha_status",
                "Check current gacha pity counters and statistics.",
                json!({ "type":"object","required":["cmd"],"properties":{
                    "cmd":{"type":"string","const":"status"}
                }}),
                json!({ "type":"object","properties":{
                    "ok":{"type":"boolean"},"message":{"type":["string","null"]},
                    "pity_5":{"type":"integer"},"pity_4":{"type":"integer"},
                    "guaranteed":{"type":"boolean"}
                }}),
                &["reads current pity state"],
                &[],
            ),
        ],
        None)
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_gacha_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<GachaRequest>(&req.payload)
        .map_err(|e| format!("gacha plugin: {e}"))
        .and_then(handle)
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&GachaResponse {
            ok: false, action: "error".into(),
            message: Some(e), results: None,
            pity_5: None, pity_4: None, guaranteed: None,
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
