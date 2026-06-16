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
use std::path::PathBuf;

// ── gacha state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GachaState {
    char_pity_5: u32,
    char_pity_4: u32,
    char_guaranteed: bool,
    char_4_guaranteed: bool,
    char_pity_5_total: u64,
    char_total_pulls: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BannerConfig {
    featured_5: String,      // current rate-up 5★
    featured_4: [String; 3], // current rate-up 4★ (3 of them)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Store {
    state: GachaState,
    banner: BannerConfig,
}

fn store_path() -> PathBuf {
    PathBuf::from("data/gacha/state.json")
}

fn load_store() -> Store {
    let path = store_path();
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Store::default()
    }
}

fn save_store(store: &Store) {
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(store) {
        let _ = std::fs::write(&path, json);
    }
}

impl Default for Store {
    fn default() -> Self {
        Store {
            state: GachaState {
                char_pity_5: 0,
                char_pity_4: 0,
                char_guaranteed: false,
                char_4_guaranteed: false,
                char_pity_5_total: 0,
                char_total_pulls: 0,
            },
            banner: BannerConfig {
                featured_5: String::new(),
                featured_4: [String::new(), String::new(), String::new()],
            },
        }
    }
}

// ── gacha constants ────────────────────────────────────────────────────

// Standard banner 5★ (常驻)
const STANDARD_5: &[&str] = &[
    "迪卢克","琴","莫娜","七七","刻晴","提纳里","迪希雅","梦见月瑞希",
];
// Limited 5★ (限定)
const LIMITED_5: &[&str] = &[
    "温迪","可莉","达达利亚","钟离","阿贝多","甘雨","魈","胡桃","优菈",
    "万叶","神里绫华","宵宫","雷电将军","珊瑚宫心海","荒泷一斗","申鹤",
    "八重神子","神里绫人","夜兰","赛诺","妮露","纳西妲","流浪者",
    "艾尔海森","白术","林尼","那维莱特","莱欧斯利","芙宁娜","娜维娅",
    "闲云","千织","阿蕾奇诺","克洛琳德","希格雯","艾梅莉埃",
    "玛拉妮","基尼奇","希诺宁","恰斯卡","玛薇卡","茜特菈莉","瓦蕾莎","爱可菲","丝柯克",
];
// All 4★ (全四星)
const ALL_4: &[&str] = &[
    "安柏","凯亚","丽莎","芭芭拉","雷泽","香菱","行秋","北斗","凝光",
    "菲谢尔","班尼特","诺艾尔","砂糖","迪奥娜","辛焱","罗莎莉亚","烟绯",
    "早柚","九条裟罗","托马","五郎","云堇","久岐忍","鹿野院平藏",
    "柯莱","多莉","坎蒂丝","莱依拉","珐露珊","瑶瑶","米卡","绮良良",
    "卡维","琳妮特","菲米尼","夏洛蒂","夏沃蕾","嘉明","卡齐娜",
    "欧洛伦","伊安珊","伊法","蓝砚","重云",
];
// 3★ weapons
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
    count: Option<u32>,
    #[serde(default)]
    args: Option<Vec<String>>,
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

fn pull_char_banner(state: &mut GachaState, banner: &BannerConfig) -> GachaResult {
    state.char_pity_5 += 1;
    state.char_pity_4 += 1;
    state.char_total_pulls += 1;

    let rate_5 = char_5_star_rate(state.char_pity_5);
    if rand_f64() < rate_5 {
        state.char_pity_5_total += state.char_pity_5 as u64;
        // 50/50: featured limited or random standard
        let (name, is_feat) = if state.char_guaranteed || rand_f64() < 0.5 {
            state.char_guaranteed = false;
            let n = if banner.featured_5.is_empty() {
                pick(STANDARD_5).to_string()
            } else {
                banner.featured_5.clone()
            };
            (n, true)
        } else {
            state.char_guaranteed = true;
            (pick(STANDARD_5).to_string(), false)
        };
        state.char_pity_5 = 0;
        state.char_pity_4 = 0; // 5★ pull also resets 4★ pity
        return GachaResult { name, stars: 5, is_featured: is_feat };
    }

    // 4★: 50% rate-up 4★ with guarantee system
    // Hard pity at 10; soft pity formula hits 1.0 at pity 9, so >= 9 is safe
    if state.char_pity_4 >= 9 || rand_f64() < char_4_star_rate(state.char_pity_4) {
        let (name, is_feat) = if !banner.featured_4[0].is_empty() {
            // 50% chance to get a featured 4★, or guaranteed if last 4★ wasn't featured
            if state.char_4_guaranteed || rand_f64() < 0.5 {
                state.char_4_guaranteed = false;
                let idx = (rand_f64() * banner.featured_4.len() as f64) as usize;
                (banner.featured_4[idx.min(banner.featured_4.len()-1)].clone(), true)
            } else {
                state.char_4_guaranteed = true;
                (pick(ALL_4).to_string(), false)
            }
        } else {
            (pick(ALL_4).to_string(), false)
        };
        state.char_pity_4 = 0;
        return GachaResult { name, stars: 4, is_featured: is_feat };
    }

    GachaResult { name: pick(CHAR_3).to_string(), stars: 3, is_featured: false }
}

// ── handlers ───────────────────────────────────────────────────────────

fn handle_pull(count: u32) -> Result<GachaResponse, String> {
    if count == 0 || count > 100 { return Err("count must be 1-100".into()); }

    let mut store = load_store();
    let mut results = Vec::new();
    for _ in 0..count {
        results.push(pull_char_banner(&mut store.state, &store.banner));
    }
    save_store(&store);

    Ok(GachaResponse {
        ok: true,
        action: "pull".into(),
        message: None,
        results: Some(results),
        pity_5: Some(store.state.char_pity_5),
        pity_4: Some(store.state.char_pity_4),
        guaranteed: Some(store.state.char_guaranteed),
    })
}

fn handle_status() -> Result<GachaResponse, String> {
    let store = load_store();
    let has_banner = !store.banner.featured_5.is_empty();
    let banner_info = if has_banner {
        format!(
            " | UP5★: {} | UP4★: {} / {} / {}",
            store.banner.featured_5, store.banner.featured_4[0],
            store.banner.featured_4[1], store.banner.featured_4[2]
        )
    } else {
        " | 未设置UP池 (用 setbanner 设置)".to_string()
    };
    Ok(GachaResponse {
        ok: true,
        action: "status".into(),
        message: Some(format!(
            "5★ pity: {}, 4★ pity: {}, 5★保底: {}, 4★保底: {}, total pulls: {}, avg 5★: {:.1}{}",
            store.state.char_pity_5, store.state.char_pity_4,
            store.state.char_guaranteed, store.state.char_4_guaranteed,
            store.state.char_total_pulls,
            if store.state.char_pity_5_total > 0 {
                store.state.char_total_pulls as f64 / store.state.char_pity_5_total as f64
            } else { 0.0 },
            banner_info,
        )),
        results: None,
        pity_5: Some(store.state.char_pity_5),
        pity_4: Some(store.state.char_pity_4),
        guaranteed: Some(store.state.char_guaranteed),
    })
}

fn handle_banner() -> Result<GachaResponse, String> {
    let store = load_store();
    if store.banner.featured_5.is_empty() {
        Ok(GachaResponse {
            ok: true, action: "banner".into(),
            message: Some("当前未设置UP池。使用 setbanner <5星> <4星1> <4星2> <4星3> 来设置".into()),
            results: None, pity_5: None, pity_4: None, guaranteed: None,
        })
    } else {
        Ok(GachaResponse {
            ok: true, action: "banner".into(),
            message: Some(format!(
                "UP5★: {} | UP4★: {} / {} / {}",
                store.banner.featured_5, store.banner.featured_4[0],
                store.banner.featured_4[1], store.banner.featured_4[2]
            )),
            results: None, pity_5: None, pity_4: None, guaranteed: None,
        })
    }
}

fn resolve_5star(name: &str) -> Option<&'static str> {
    if LIMITED_5.contains(&name) || STANDARD_5.contains(&name) {
        for c in LIMITED_5.iter().chain(STANDARD_5.iter()) {
            if *c == name { return Some(c); }
        }
    }
    // Strip prefix before and including "·" (e.g. "火神·玛薇卡" → "玛薇卡")
    if let Some(pos) = name.find('·') {
        let stripped: &str = &name[pos + '·'.len_utf8()..];
        for c in LIMITED_5.iter().chain(STANDARD_5.iter()) {
            if *c == stripped { return Some(c); }
        }
    }
    None
}

fn resolve_4star(name: &str) -> Option<&'static str> {
    if ALL_4.contains(&name) {
        for c in ALL_4.iter() {
            if *c == name { return Some(c); }
        }
    }
    if let Some(pos) = name.find('·') {
        let stripped: &str = &name[pos + '·'.len_utf8()..];
        for c in ALL_4.iter() {
            if *c == stripped { return Some(c); }
        }
    }
    None
}

fn handle_setbanner(args: &[String]) -> Result<GachaResponse, String> {
    if args.len() < 4 {
        return Err("用法: setbanner <5星角色> <4星1> <4星2> <4星3>".into());
    }
    let f5_raw = &args[0];
    let f4_0_raw = &args[1];
    let f4_1_raw = &args[2];
    let f4_2_raw = &args[3];

    let f5 = resolve_5star(f5_raw)
        .ok_or_else(|| format!("未知5星角色: {f5_raw}。用 list 查看可用角色"))?
        .to_string();
    let f4_0 = resolve_4star(f4_0_raw)
        .ok_or_else(|| format!("未知4星角色: {f4_0_raw}。用 list 查看可用角色"))?
        .to_string();
    let f4_1 = resolve_4star(f4_1_raw)
        .ok_or_else(|| format!("未知4星角色: {f4_1_raw}。用 list 查看可用角色"))?
        .to_string();
    let f4_2 = resolve_4star(f4_2_raw)
        .ok_or_else(|| format!("未知4星角色: {f4_2_raw}。用 list 查看可用角色"))?
        .to_string();

    let mut store = load_store();
    store.banner.featured_5 = f5.clone();
    store.banner.featured_4 = [f4_0.clone(), f4_1.clone(), f4_2.clone()];
    save_store(&store);

    Ok(GachaResponse {
        ok: true, action: "setbanner".into(),
        message: Some(format!("UP池已设置: 5★ {} | 4★ {} / {} / {}", f5, f4_0, f4_1, f4_2)),
        results: None, pity_5: None, pity_4: None, guaranteed: None,
    })
}

fn handle_list() -> Result<GachaResponse, String> {
    let mut msg = String::from("【5星常驻】");
    for c in STANDARD_5 { msg.push_str(c); msg.push(' '); }
    msg.push_str("\n【5星限定】");
    for c in LIMITED_5 { msg.push_str(c); msg.push(' '); }
    msg.push_str("\n【4星全角色】");
    for c in ALL_4 { msg.push_str(c); msg.push(' '); }
    Ok(GachaResponse {
        ok: true, action: "list".into(),
        message: Some(msg),
        results: None, pity_5: None, pity_4: None, guaranteed: None,
    })
}

fn handle_reset() -> Result<GachaResponse, String> {
    let mut store = load_store();
    store.state = GachaState {
        char_pity_5: 0, char_pity_4: 0, char_guaranteed: false, char_4_guaranteed: false,
        char_pity_5_total: 0, char_total_pulls: 0,
    };
    save_store(&store);
    Ok(GachaResponse {
        ok: true, action: "reset".into(),
        message: Some("保底数据已重置".into()),
        results: None, pity_5: Some(0), pity_4: Some(0), guaranteed: Some(false),
    })
}

fn handle(req: GachaRequest) -> Result<GachaResponse, String> {
    match req.cmd.as_str() {
        "pull" => handle_pull(req.count.unwrap_or(1)),
        "status" => handle_status(),
        "banner" => handle_banner(),
        "setbanner" => {
            let args: Vec<String> = req.args.unwrap_or_default();
            handle_setbanner(&args)
        }
        "list" => handle_list(),
        "reset" => handle_reset(),
        other => Err(format!("未知命令: {other}。可用: pull, status, banner, setbanner, list, reset")),
    }
}

// ── plugin api ─────────────────────────────────────────────────────────

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "gacha","gacha","0.1.0",
        Some("Gacha"),
        vec![
            node_doc("gacha_entry",
                "原神抽卡模拟器。支持自定义UP池，完整角色库，精准保底与50/50机制。\n命令: pull [N] | status | banner | setbanner <5星> <4星> <4星> <4星> | list | reset",
                json!({ "type":"object","required":["cmd"],"properties":{
                    "cmd":{"type":"string","description":"pull | status | banner | setbanner | list | reset"},
                    "count":{"type":"integer","description":"抽数 1-100, 默认1"},
                    "args":{"type":"array","items":{"type":"string"},"description":"setbanner参数: [5星, 4星1, 4星2, 4星3]"}
                }}),
                json!({ "type":"object","properties":{
                    "ok":{"type":"boolean"},"action":{"type":"string"},
                    "message":{"type":["string","null"]},
                    "results":{"type":"array","items":{"type":"object"}},
                    "pity_5":{"type":"integer"},"pity_4":{"type":"integer"},
                    "guaranteed":{"type":"boolean"}
                }}),
                &["accurate Genshin pity model","soft pity starts at 73","50/50 system","custom banner support","complete character roster"],
                &["invalid count","unknown character name"],
            ).with_agent_accessible(),
            node_doc("gacha_status",
                "查看当前保底计数、UP池信息及抽卡统计。",
                json!({ "type":"object","required":["cmd"],"properties":{
                    "cmd":{"type":"string","const":"status"}
                }}),
                json!({ "type":"object","properties":{
                    "ok":{"type":"boolean"},"message":{"type":["string","null"]},
                    "pity_5":{"type":"integer"},"pity_4":{"type":"integer"},
                    "guaranteed":{"type":"boolean"}
                }}),
                &["reads current pity + banner state"],
                &[],
            ).with_agent_accessible(),
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
