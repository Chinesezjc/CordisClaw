//! Gacha / wish simulator plugin — Genshin Impact probability model.
//!
//! Nodes:
//! - gacha_entry — pull wishes (single / 10-pull)
//! - gacha_status — check current pity counters

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

// ── gacha state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GachaState {
    char_pity_5: u32,
    char_pity_4: u32,
    char_guaranteed: bool,
    char_4_guaranteed: bool,
    /// Cumulative sum of pity-5 counts at every 5★ trigger. Retained for
    /// backwards compat; the correct-average denominator is
    /// `char_5_star_count` (P2-39 adds it below).
    char_pity_5_total: u64,
    char_total_pulls: u64,
    /// P2-39: number of 5★ pulls seen since last reset. The historical
    /// "avg 5★" formula computed `total_pulls / char_pity_5_total`, which
    /// is a ratio of counts-to-gap-sum ≈ 1 for balanced RNG — always wrong.
    /// The correct denominator is the count of 5★ events, tracked here.
    #[serde(default)]
    char_5_star_count: u64,
    /// P3: featured/off-banner accounting, so a report can never be built by
    /// matching names again (the standard 4★ pool contains the featured
    /// names, so name matching conflates the two).
    #[serde(default)]
    char_5_featured_count: u64,
    #[serde(default)]
    char_4_featured_count: u64,
    #[serde(default)]
    char_4_off_banner_count: u64,
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

/// Where the runtime keeps gacha data (`data/gacha`), resolved like before:
/// `$CORDIS_FIXTURES_ROOT/../data/gacha`, else the historical relative path.
fn resolve_data_dir() -> PathBuf {
    if let Ok(root) = std::env::var("CORDIS_FIXTURES_ROOT") {
        let ws = PathBuf::from(&root)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(&root));
        return ws.join("data/gacha");
    }
    PathBuf::from("data/gacha")
}

/// A player id safe to use as a file name.
fn sanitize_uid(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

/// The banner every player gets unless they set their own: the real Genshin
/// rate-up this plugin was last updated for. `data/gacha/banner.json`
/// overrides it (what an operator's `setbanner` writes), so rotation does not
/// need a rebuild.
fn real_banner() -> BannerConfig {
    BannerConfig {
        featured_5: "玛薇卡".to_string(),
        featured_4: [
            "班尼特".to_string(),
            "迪奥娜".to_string(),
            "卡齐娜".to_string(),
        ],
    }
}

fn banner_path(dir: &std::path::Path) -> PathBuf {
    dir.join("banner.json")
}

/// The default banner: `banner.json` when present, else the built-in one.
fn default_banner(dir: &std::path::Path) -> BannerConfig {
    std::fs::read_to_string(banner_path(dir))
        .ok()
        .and_then(|text| serde_json::from_str::<BannerConfig>(&text).ok())
        .filter(|banner| !banner.featured_5.trim().is_empty())
        .unwrap_or_else(real_banner)
}

fn user_store_path(dir: &std::path::Path, uid: &str) -> PathBuf {
    dir.join("users")
        .join(format!("{}.json", sanitize_uid(uid)))
}

/// Legacy single-player file; used as the `default` player's store until it is
/// written once (then the per-user file takes over).
fn legacy_store_path(dir: &std::path::Path) -> PathBuf {
    dir.join("state.json")
}

impl Store {
    fn with_banner(banner: BannerConfig) -> Self {
        Store {
            state: GachaState::default(),
            banner,
        }
    }
}

/// P1-44: sticky flag set when a load failure would otherwise be masked;
/// prevents saves from clobbering the on-disk file until an operator
/// intervenes.
static SAVE_BLOCKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// P1-44: process-wide lock around read-modify-write of the gacha state.
/// Concurrent pull invocations used to interleave `load → mutate → save`,
/// losing pity increments. This is a coarse mutex (whole state), which is
/// fine because gacha is a handful of small files.
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn load_store_at(path: &std::path::Path, fallback_banner: BannerConfig) -> Store {
    if !path.exists() {
        return Store::with_banner(fallback_banner);
    }
    // P1-44: `.ok()` used to silently swallow deserialize failures and reset
    // state to default (nuking user progress). Log the parse error and block
    // further saves instead.
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Store>(&text) {
            Ok(store) => store,
            Err(err) => {
                eprintln!(
                    "[gacha] state file at {} failed to parse ({err}); \
                     refusing to overwrite — please repair or delete it",
                    path.display()
                );
                SAVE_BLOCKED.store(true, std::sync::atomic::Ordering::SeqCst);
                Store::with_banner(fallback_banner)
            }
        },
        Err(err) => {
            eprintln!("[gacha] state file read failed: {err}");
            Store::with_banner(fallback_banner)
        }
    }
}

/// One player's store: their own file, else the legacy file (only for the
/// `default` player), else a fresh store on the default banner.
fn load_user_store(dir: &std::path::Path, uid: &str) -> Store {
    let path = user_store_path(dir, uid);
    let fallback = default_banner(dir);
    if path.exists() {
        return load_store_at(&path, fallback);
    }
    let legacy = legacy_store_path(dir);
    if sanitize_uid(uid) == "default" && legacy.exists() {
        return load_store_at(&legacy, fallback);
    }
    Store::with_banner(fallback)
}

fn write_json_atomic(path: &std::path::Path, value: &impl Serialize) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(json) = serde_json::to_string(value) else {
        return;
    };
    // P1-44: atomic tmp + rename so a crash mid-write doesn't leave a
    // truncated file (which would then trip the load-poison path on boot).
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if let Err(err) = std::fs::write(&tmp, &json) {
        eprintln!("[gacha] save tmp write failed: {err}");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        eprintln!("[gacha] save rename failed: {err}");
        let _ = std::fs::remove_file(&tmp);
    }
}

fn save_user_store(dir: &std::path::Path, uid: &str, store: &Store) {
    if SAVE_BLOCKED.load(std::sync::atomic::Ordering::SeqCst) {
        eprintln!("[gacha] save blocked because prior load failed to parse");
        return;
    }
    write_json_atomic(&user_store_path(dir, uid), store);
}

fn save_default_banner(dir: &std::path::Path, banner: &BannerConfig) {
    write_json_atomic(&banner_path(dir), banner);
}

// ── caller identity / permission ────────────────────────────────────────

/// Administrators allowed to change the DEFAULT banner. Reuses the QQ
/// operator list (`data/qq_operators.json`) so there is one place to manage
/// who may do this.
fn operator_allowlist(dir: &std::path::Path) -> Vec<String> {
    let path = dir
        .parent()
        .map(|p| p.join("qq_operators.json"))
        .unwrap_or_else(|| dir.join("qq_operators.json"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("operators")
        .and_then(|ops| ops.as_array())
        .map(|ops| {
            ops.iter()
                .filter_map(|op| op.as_str())
                .map(sanitize_uid)
                .collect()
        })
        .unwrap_or_default()
}

/// The invoking human, from the kernel-injected `__meta` (never from the model).
fn meta_actor_id(req: &GachaRequest) -> Option<String> {
    let actor = req
        .meta
        .as_ref()?
        .get("actor")?
        .as_str()?
        .rsplit(':')
        .next()
        .unwrap_or_default();
    let id = sanitize_uid(actor.trim());
    (id != "default").then_some(id)
}

/// No `__meta` (CLI / ops invoke) counts as an operator; otherwise the actor
/// must be on the operator list.
fn caller_is_operator(req: &GachaRequest, dir: &std::path::Path) -> bool {
    match req.meta {
        None => true,
        Some(_) => meta_actor_id(req)
            .map(|actor| operator_allowlist(dir).iter().any(|op| op == &actor))
            .unwrap_or(false),
    }
}

/// A player may only touch their own pool: an explicit `user` is honoured for
/// operators only, everyone else is pinned to their `__meta` identity.
fn caller_uid(req: &GachaRequest, dir: &std::path::Path) -> String {
    if caller_is_operator(req, dir) {
        if let Some(user) = req.user.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            return sanitize_uid(user);
        }
    }
    meta_actor_id(req).unwrap_or_else(|| "default".to_string())
}

// ── gacha constants ────────────────────────────────────────────────────

// Standard banner 5★ (常驻)
const STANDARD_5: &[&str] = &[
    "迪卢克",
    "琴",
    "莫娜",
    "七七",
    "刻晴",
    "提纳里",
    "迪希雅",
    "梦见月瑞希",
];
// Limited 5★ (限定)
const LIMITED_5: &[&str] = &[
    "温迪",
    "可莉",
    "达达利亚",
    "钟离",
    "阿贝多",
    "甘雨",
    "魈",
    "胡桃",
    "优菈",
    "万叶",
    "神里绫华",
    "宵宫",
    "雷电将军",
    "珊瑚宫心海",
    "荒泷一斗",
    "申鹤",
    "八重神子",
    "神里绫人",
    "夜兰",
    "赛诺",
    "妮露",
    "纳西妲",
    "流浪者",
    "艾尔海森",
    "白术",
    "林尼",
    "那维莱特",
    "莱欧斯利",
    "芙宁娜",
    "娜维娅",
    "闲云",
    "千织",
    "阿蕾奇诺",
    "克洛琳德",
    "希格雯",
    "艾梅莉埃",
    "玛拉妮",
    "基尼奇",
    "希诺宁",
    "恰斯卡",
    "玛薇卡",
    "茜特菈莉",
    "瓦蕾莎",
    "爱可菲",
    "丝柯克",
];
// All 4★ (全四星)
const ALL_4: &[&str] = &[
    "安柏",
    "凯亚",
    "丽莎",
    "芭芭拉",
    "雷泽",
    "香菱",
    "行秋",
    "北斗",
    "凝光",
    "菲谢尔",
    "班尼特",
    "诺艾尔",
    "砂糖",
    "迪奥娜",
    "辛焱",
    "罗莎莉亚",
    "烟绯",
    "早柚",
    "九条裟罗",
    "托马",
    "五郎",
    "云堇",
    "久岐忍",
    "鹿野院平藏",
    "柯莱",
    "多莉",
    "坎蒂丝",
    "莱依拉",
    "珐露珊",
    "瑶瑶",
    "米卡",
    "绮良良",
    "卡维",
    "琳妮特",
    "菲米尼",
    "夏洛蒂",
    "夏沃蕾",
    "嘉明",
    "卡齐娜",
    "欧洛伦",
    "伊安珊",
    "伊法",
    "蓝砚",
    "重云",
];
// 3★ weapons
const CHAR_3: &[&str] = &[
    "弹弓",
    "飞天御剑",
    "黑缨枪",
    "铁影阔剑",
    "魔导绪论",
    "讨龙英杰谭",
    "黎明神剑",
];

fn rand_f64() -> f64 {
    rand::random::<f64>()
}

fn char_5_star_rate(pity: u32) -> f64 {
    if pity < 73 {
        0.006
    } else {
        (0.006 + (pity - 72) as f64 * 0.06).min(1.0)
    }
}

fn char_4_star_rate(pity: u32) -> f64 {
    if pity < 8 {
        0.051
    } else {
        (0.051 + (pity - 7) as f64 * 0.51).min(1.0)
    }
}

fn pick<T: Copy>(arr: &[T]) -> T {
    let idx = (rand_f64() * arr.len() as f64) as usize;
    arr[idx.min(arr.len() - 1)]
}

// ── request / response types ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GachaRequest {
    /// P1-43: `node_id` restricts what `cmd` values are legal for this
    /// invocation. The `gacha_status` node advertises `cmd:"status"` in
    /// its schema; without an explicit check the plugin used to accept
    /// `{cmd:"reset"}` here and happily wipe pity. Route each node to a
    /// whitelist of commands so schema drift stays honest.
    #[serde(default)]
    node_id: Option<String>,
    cmd: String,
    #[serde(default)]
    count: Option<u32>,
    #[serde(default)]
    args: Option<Vec<String>>,
    /// Optional target player. Honoured for operators only — everyone else is
    /// pinned to their `__meta` identity, so nobody can edit another player's
    /// pool.
    #[serde(default)]
    user: Option<String>,
    /// Kernel-injected identity for agent invocations (`{actor, target}`).
    /// The model cannot forge it, so it is the authorization source.
    #[serde(default, rename = "__meta")]
    meta: Option<serde_json::Value>,
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
    /// Which player's pool this response is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    uid: Option<String>,
    /// Featured / off-banner counters, so a report cannot be built by matching
    /// character names (the standard 4★ pool contains the featured names).
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<serde_json::Value>,
    /// The pool this player pulls on.
    #[serde(skip_serializing_if = "Option::is_none")]
    banner: Option<serde_json::Value>,
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
        state.char_5_star_count += 1;
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
        if is_feat {
            state.char_5_featured_count += 1;
        }
        state.char_pity_5 = 0;
        state.char_pity_4 = 0; // 5★ pull also resets 4★ pity
        return GachaResult {
            name,
            stars: 5,
            is_featured: is_feat,
        };
    }

    // 4★: 50% rate-up 4★ with guarantee system
    // Hard pity at 10; soft pity formula hits 1.0 at pity 9, so >= 9 is safe
    if state.char_pity_4 >= 9 || rand_f64() < char_4_star_rate(state.char_pity_4) {
        let (name, is_feat) = if !banner.featured_4[0].is_empty() {
            // 50% chance to get a featured 4★, or guaranteed if last 4★ wasn't featured
            if state.char_4_guaranteed || rand_f64() < 0.5 {
                state.char_4_guaranteed = false;
                let idx = (rand_f64() * banner.featured_4.len() as f64) as usize;
                (
                    banner.featured_4[idx.min(banner.featured_4.len() - 1)].clone(),
                    true,
                )
            } else {
                state.char_4_guaranteed = true;
                (pick(ALL_4).to_string(), false)
            }
        } else {
            (pick(ALL_4).to_string(), false)
        };
        if is_feat {
            state.char_4_featured_count += 1;
        } else {
            state.char_4_off_banner_count += 1;
        }
        state.char_pity_4 = 0;
        return GachaResult {
            name,
            stars: 4,
            is_featured: is_feat,
        };
    }

    GachaResult {
        name: pick(CHAR_3).to_string(),
        stars: 3,
        is_featured: false,
    }
}

// ── handlers ───────────────────────────────────────────────────────────

fn banner_line(banner: &BannerConfig) -> String {
    format!(
        "UP5★: {} | UP4★: {} / {} / {}",
        banner.featured_5, banner.featured_4[0], banner.featured_4[1], banner.featured_4[2]
    )
}

fn stats_value(store: &Store) -> serde_json::Value {
    json!({
        "uid": serde_json::Value::Null,
        "featured_5_count": store.state.char_5_featured_count,
        "featured_4_count": store.state.char_4_featured_count,
        "off_banner_4_count": store.state.char_4_off_banner_count,
    })
}

fn handle_pull(dir: &std::path::Path, uid: &str, count: u32) -> Result<GachaResponse, String> {
    if count == 0 || count > 100 {
        return Err("count must be 1-100".into());
    }

    // P1-44: hold STATE_LOCK across the whole read-modify-write so
    // concurrent pulls don't lose pity or duplicate results.
    let _guard = STATE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = load_user_store(dir, uid);
    let mut results = Vec::new();
    for _ in 0..count {
        results.push(pull_char_banner(&mut store.state, &store.banner));
    }
    save_user_store(dir, uid, &store);

    Ok(GachaResponse {
        ok: true,
        action: "pull".into(),
        message: None,
        results: Some(results),
        pity_5: Some(store.state.char_pity_5),
        pity_4: Some(store.state.char_pity_4),
        guaranteed: Some(store.state.char_guaranteed),
        uid: Some(uid.to_string()),
        stats: Some(stats_value(&store)),
        banner: None,
    })
}

fn handle_status(dir: &std::path::Path, uid: &str) -> Result<GachaResponse, String> {
    let store = load_user_store(dir, uid);
    Ok(GachaResponse {
        ok: true,
        action: "status".into(),
        message: Some(format!(
            "[{uid}] 5★ pity: {}, 4★ pity: {}, 5★保底: {}, 4★保底: {}, total pulls: {}, avg 5★: {:.1} | {} | UP 5★: {} / 4★: {}",
            store.state.char_pity_5,
            store.state.char_pity_4,
            store.state.char_guaranteed,
            store.state.char_4_guaranteed,
            store.state.char_total_pulls,
            // P2-39: correct average is total_pulls / count_of_5star.
            if store.state.char_5_star_count > 0 {
                store.state.char_total_pulls as f64 / store.state.char_5_star_count as f64
            } else {
                0.0
            },
            banner_line(&store.banner),
            store.state.char_5_featured_count,
            store.state.char_4_featured_count,
        )),
        results: None,
        pity_5: Some(store.state.char_pity_5),
        pity_4: Some(store.state.char_pity_4),
        guaranteed: Some(store.state.char_guaranteed),
        uid: Some(uid.to_string()),
        stats: Some(stats_value(&store)),
        banner: Some(json!({
            "featured_5": store.banner.featured_5,
            "featured_4": store.banner.featured_4,
        })),
    })
}

fn handle_banner(dir: &std::path::Path, uid: &str) -> Result<GachaResponse, String> {
    let store = load_user_store(dir, uid);
    Ok(GachaResponse {
        ok: true,
        action: "banner".into(),
        message: Some(banner_line(&store.banner)),
        results: None,
        pity_5: None,
        pity_4: None,
        guaranteed: None,
        uid: Some(uid.to_string()),
        stats: None,
        banner: Some(json!({
            "featured_5": store.banner.featured_5,
            "featured_4": store.banner.featured_4,
        })),
    })
}

fn resolve_5star(name: &str) -> Option<&'static str> {
    if LIMITED_5.contains(&name) || STANDARD_5.contains(&name) {
        for c in LIMITED_5.iter().chain(STANDARD_5.iter()) {
            if *c == name {
                return Some(c);
            }
        }
    }
    // Strip prefix before and including "·" (e.g. "火神·玛薇卡" → "玛薇卡")
    if let Some(pos) = name.find('·') {
        let stripped: &str = &name[pos + '·'.len_utf8()..];
        for c in LIMITED_5.iter().chain(STANDARD_5.iter()) {
            if *c == stripped {
                return Some(c);
            }
        }
    }
    None
}

fn resolve_4star(name: &str) -> Option<&'static str> {
    if ALL_4.contains(&name) {
        for c in ALL_4.iter() {
            if *c == name {
                return Some(c);
            }
        }
    }
    if let Some(pos) = name.find('·') {
        let stripped: &str = &name[pos + '·'.len_utf8()..];
        for c in ALL_4.iter() {
            if *c == stripped {
                return Some(c);
            }
        }
    }
    None
}

/// `setbanner` changes the DEFAULT pool for operators (so it follows the real
/// game banner) and the caller's OWN pool for everyone else. A player can
/// never touch another player's pool: their identity comes from `__meta`, and
/// an explicit `user` argument is honoured for operators only.
fn handle_setbanner(
    dir: &std::path::Path,
    uid: &str,
    operator: bool,
    args: &[String],
) -> Result<GachaResponse, String> {
    if args.len() < 4 {
        return Err("用法: setbanner <5星角色> <4星1> <4星2> <4星3>".into());
    }
    let f5 = resolve_5star(&args[0])
        .ok_or_else(|| format!("未知5星角色: {}。用 list 查看可用角色", args[0]))?
        .to_string();
    let f4 = [
        resolve_4star(&args[1])
            .ok_or_else(|| format!("未知4星角色: {}。用 list 查看可用角色", args[1]))?
            .to_string(),
        resolve_4star(&args[2])
            .ok_or_else(|| format!("未知4星角色: {}。用 list 查看可用角色", args[2]))?
            .to_string(),
        resolve_4star(&args[3])
            .ok_or_else(|| format!("未知4星角色: {}。用 list 查看可用角色", args[3]))?
            .to_string(),
    ];
    let banner = BannerConfig {
        featured_5: f5.clone(),
        featured_4: f4.clone(),
    };

    let _guard = STATE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let message = if operator {
        // The default pool, shared by players who never set their own.
        save_default_banner(dir, &banner);
        // Keep the operator's own pool in sync so their next pull uses it.
        let mut store = load_user_store(dir, uid);
        store.banner = banner.clone();
        save_user_store(dir, uid, &store);
        format!(
            "默认UP池已设置（操作员）: {f5} | 4★ {} / {} / {}",
            f4[0], f4[1], f4[2]
        )
    } else {
        let mut store = load_user_store(dir, uid);
        store.banner = banner.clone();
        save_user_store(dir, uid, &store);
        format!(
            "你的UP池已设置: {f5} | 4★ {} / {} / {}",
            f4[0], f4[1], f4[2]
        )
    };
    Ok(GachaResponse {
        ok: true,
        action: "setbanner".into(),
        message: Some(message),
        results: None,
        pity_5: None,
        pity_4: None,
        guaranteed: None,
        uid: Some(uid.to_string()),
        stats: None,
        banner: Some(json!({
            "featured_5": banner.featured_5,
            "featured_4": banner.featured_4,
        })),
    })
}

fn handle_list() -> Result<GachaResponse, String> {
    let mut msg = String::from("【5星常驻】");
    for c in STANDARD_5 {
        msg.push_str(c);
        msg.push(' ');
    }
    msg.push_str("\n【5星限定】");
    for c in LIMITED_5 {
        msg.push_str(c);
        msg.push(' ');
    }
    msg.push_str("\n【4星全角色】");
    for c in ALL_4 {
        msg.push_str(c);
        msg.push(' ');
    }
    Ok(GachaResponse {
        ok: true,
        action: "list".into(),
        message: Some(msg),
        results: None,
        pity_5: None,
        pity_4: None,
        guaranteed: None,
        uid: None,
        stats: None,
        banner: None,
    })
}

fn handle_reset(dir: &std::path::Path, uid: &str) -> Result<GachaResponse, String> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = load_user_store(dir, uid);
    store.state = GachaState::default();
    save_user_store(dir, uid, &store);
    Ok(GachaResponse {
        ok: true,
        action: "reset".into(),
        message: Some(format!("[{uid}] 保底数据已重置")),
        results: None,
        pity_5: Some(0),
        pity_4: Some(0),
        guaranteed: Some(false),
        uid: Some(uid.to_string()),
        stats: None,
        banner: None,
    })
}

fn handle_in(dir: &std::path::Path, req: GachaRequest) -> Result<GachaResponse, String> {
    // P1-43: enforce per-node command whitelist. gacha_status must only
    // ever return status; gacha_entry accepts the full command set.
    if let Some(node) = req.node_id.as_deref() {
        match node {
            "gacha_status" => {
                if req.cmd.as_str() != "status" {
                    return Err(format!(
                        "gacha_status only supports cmd=status; got cmd={}",
                        req.cmd
                    ));
                }
            }
            "gacha_entry" | "" => {}
            other => {
                return Err(format!("unknown gacha node_id: {other}"));
            }
        }
    }
    let uid = caller_uid(&req, dir);
    let operator = caller_is_operator(&req, dir);
    match req.cmd.as_str() {
        "pull" => handle_pull(dir, &uid, req.count.unwrap_or(1)),
        "status" => handle_status(dir, &uid),
        "banner" => handle_banner(dir, &uid),
        "setbanner" => handle_setbanner(dir, &uid, operator, &req.args.unwrap_or_default()),
        "list" => handle_list(),
        "reset" => handle_reset(dir, &uid),
        other => Err(format!(
            "未知命令: {other}。可用: pull, status, banner, setbanner, list, reset"
        )),
    }
}

fn handle(req: GachaRequest) -> Result<GachaResponse, String> {
    let dir = resolve_data_dir();
    handle_in(&dir, req)
}

// ── plugin api ─────────────────────────────────────────────────────────

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "gacha","gacha","0.1.0",
        Some("Gacha"),
        vec![
            node_doc("gacha_entry",
                "原神抽卡模拟器。每个玩家有自己的池子和保底（按 QQ 号区分）：默认池是当前真实卡池，玩家可以用 setbanner 换自己的 UP；操作员改的是默认池。\n命令: pull [N] | status | banner | setbanner <5星> <4星> <4星> <4星> | list | reset\n返回里的 stats.featured_4_count / off_banner_4_count 是权威的四星 UP/歪 计数——不要用角色名字去数（常驻四星池包含 UP 名字，按名字统计会重复计入）。",
                json!({ "type":"object","required":["cmd"],"properties":{
                    "cmd":{"type":"string","description":"pull | status | banner | setbanner | list | reset"},
                    "count":{"type":"integer","description":"抽数 1-100, 默认1"},
                    "args":{"type":"array","items":{"type":"string"},"description":"setbanner参数: [5星, 4星1, 4星2, 4星3]；普通玩家只改自己的池，操作员（data/qq_operators.json）改默认池"},
                    "user":{"type":"string","description":"仅操作员可用：指定要改的用户池；普通玩家的身份由内核注入，不能指定别人"}
                }}),
                json!({ "type":"object","properties":{
                    "ok":{"type":"boolean"},"action":{"type":"string"},
                    "message":{"type":["string","null"]},
                    "results":{"type":"array","items":{"type":"object"}},
                    "pity_5":{"type":"integer"},"pity_4":{"type":"integer"},
                    "guaranteed":{"type":"boolean"}
                }}),
                &["per-player pools and pity, keyed by QQ id","default banner follows the real rate-up (setbanner by an operator)","accurate Genshin pity model","soft pity starts at 73","50/50 system with 4-star guarantee (75% featured overall)","complete character roster"],
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
    AbiFingerprint::current_build("crate_gacha_v1", "api_v2")
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<GachaRequest>(&req.payload)
        .map_err(|e| format!("gacha plugin: {e}"))
        .and_then(handle)
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&GachaResponse {
            ok: false,
            action: "error".into(),
            message: Some(e),
            results: None,
            pity_5: None,
            pity_4: None,
            guaranteed: None,
            uid: None,
            stats: None,
            banner: None,
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}

#[cfg(test)]
mod tests {
    use super::{handle, GachaRequest};

    fn req(node_id: Option<&str>, cmd: &str) -> GachaRequest {
        GachaRequest {
            node_id: node_id.map(|s| s.to_string()),
            cmd: cmd.to_string(),
            count: None,
            args: None,
            user: None,
            meta: None,
        }
    }

    /// A request from a specific QQ user (kernel-injected `__meta`).
    fn user_req(uid: &str, cmd: &str) -> GachaRequest {
        GachaRequest {
            meta: Some(serde_json::json!({ "actor": format!("qq:{uid}") })),
            ..req(Some("gacha_entry"), cmd)
        }
    }

    fn setbanner_req(meta: Option<&str>, names: [&str; 4]) -> GachaRequest {
        GachaRequest {
            args: Some(names.iter().map(|n| n.to_string()).collect()),
            meta: meta.map(|uid| serde_json::json!({ "actor": format!("qq:{uid}") })),
            ..req(Some("gacha_entry"), "setbanner")
        }
    }

    fn temp_gacha_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gacha-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp gacha dir");
        dir
    }

    // Per-player pools: two players keep independent pity/banner, the default
    // banner comes from the built-in real-banner constant, and only operators
    // change the default (players may change their own).
    #[test]
    fn per_player_pools_are_independent_and_banner_comes_from_the_real_default() {
        let dir = temp_gacha_dir("pools");
        // Operator (no __meta) sets the default pool.
        let r = super::handle_in(
            &dir,
            setbanner_req(None, ["玛薇卡", "班尼特", "迪奥娜", "卡齐娜"]),
        )
        .expect("operator setbanner");
        assert!(r.ok);
        let msg = r.message.unwrap_or_default();
        assert!(msg.contains("默认UP池已设置"), "message: {msg}");

        // A fresh player inherits that default.
        let status = super::handle_in(&dir, user_req("111", "status")).expect("status");
        let banner = status.banner.expect("banner");
        assert_eq!(banner["featured_5"], "玛薇卡");
        assert_eq!(banner["featured_4"][2], "卡齐娜");

        // Player 111 pulls; player 222's pity must stay untouched.
        super::handle_in(
            &dir,
            GachaRequest {
                count: Some(10),
                ..user_req("111", "pull")
            },
        )
        .expect("pull");
        let other = super::handle_in(&dir, user_req("222", "status")).expect("status");
        assert_eq!(other.pity_5, Some(0));
        assert_eq!(other.pity_4, Some(0));

        // Player 222 sets their own pool: default stays, their own changes.
        let own = super::handle_in(
            &dir,
            setbanner_req(Some("222"), ["那维莱特", "行秋", "香菱", "班尼特"]),
        )
        .expect("player setbanner");
        let own_msg = own.message.unwrap_or_default();
        assert!(own_msg.contains("你的UP池已设置"), "message: {own_msg}");
        let p222 = super::handle_in(&dir, user_req("222", "status")).expect("status");
        assert_eq!(
            p222.banner.as_ref().expect("banner")["featured_5"],
            "那维莱特"
        );
        let p111 = super::handle_in(&dir, user_req("111", "status")).expect("status");
        assert_eq!(
            p111.banner.as_ref().expect("banner")["featured_5"],
            "玛薇卡"
        );

        // A plain player cannot target someone else via `user`.
        let hijack = GachaRequest {
            user: Some("111".to_string()),
            ..setbanner_req(Some("333"), ["丝柯克", "坎蒂丝", "米卡", "香菱"])
        };
        super::handle_in(&dir, hijack).expect("player setbanner");
        let p111 = super::handle_in(&dir, user_req("111", "status")).expect("status");
        assert_eq!(
            p111.banner.as_ref().expect("banner")["featured_5"],
            "玛薇卡",
            "a player must not be able to edit another player's pool"
        );

        // The operator list gates default changes: a player's setbanner must
        // not touch banner.json.
        let banner_file = dir.join("banner.json");
        let before = std::fs::read_to_string(&banner_file).expect("default banner file");
        let player = setbanner_req(Some("333"), ["丝柯克", "坎蒂丝", "米卡", "香菱"]);
        super::handle_in(&dir, player).expect("player setbanner");
        let after = std::fs::read_to_string(&banner_file).expect("default banner file");
        assert_eq!(before, after, "a player must not change the default pool");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The featured/off-banner counters must add up to the 4★ total, so nobody
    // can produce an off-rate report by matching names again.
    #[test]
    fn featured_counters_match_the_four_star_total() {
        let dir = temp_gacha_dir("counters");
        let r = super::handle_in(
            &dir,
            GachaRequest {
                count: Some(100),
                ..req(Some("gacha_entry"), "pull")
            },
        )
        .expect("pull");
        let stats = r.stats.expect("stats");
        let four_star = r
            .results
            .as_ref()
            .expect("results")
            .iter()
            .filter(|item| item.stars == 4)
            .count() as u64;
        assert_eq!(
            stats["featured_4_count"].as_u64().unwrap_or(0)
                + stats["off_banner_4_count"].as_u64().unwrap_or(0),
            four_star,
            "stats: {stats}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A legacy single-player state file still seeds the `default` player.
    #[test]
    fn legacy_state_file_seeds_the_default_player() {
        let dir = temp_gacha_dir("legacy");
        std::fs::write(
            dir.join("state.json"),
            r#"{"state":{"char_pity_5":42,"char_pity_4":3,"char_guaranteed":true,"char_4_guaranteed":false,"char_pity_5_total":7,"char_total_pulls":99,"char_5_star_count":2},"banner":{"featured_5":"丝柯克","featured_4":["坎蒂丝","米卡","香菱"]}}"#,
        )
        .expect("legacy state");
        let status = super::handle_in(&dir, req(None, "status")).expect("status");
        assert_eq!(status.pity_5, Some(42));
        assert_eq!(
            status.banner.as_ref().expect("banner")["featured_5"],
            "丝柯克"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-43: `gacha_status` node must reject any command other than
    /// `status`. Without the whitelist an agent using `gacha_status`
    /// could send `{cmd:"reset"}` and wipe pity state — a real
    /// regression demonstrated in the review.
    #[test]
    fn gacha_status_node_rejects_reset() {
        let err = handle(req(Some("gacha_status"), "reset")).unwrap_err();
        assert!(err.contains("gacha_status"), "unexpected: {err}");
    }

    #[test]
    fn gacha_status_node_rejects_pull() {
        let err = handle(req(Some("gacha_status"), "pull")).unwrap_err();
        assert!(err.contains("gacha_status"));
    }

    #[test]
    fn gacha_entry_node_accepts_all_commands() {
        // Just verify the whitelist gate doesn't reject at the entry
        // node — the underlying handle may still fail for other reasons
        // (state file, etc.) but the P1-43 guard must not fire.
        for cmd in &["status", "banner", "list"] {
            let r = handle(req(Some("gacha_entry"), cmd));
            // status/banner/list are read-only; they shouldn't hit the
            // node-id whitelist error path even if the state file
            // isn't present (they default state).
            if let Err(e) = &r {
                assert!(!e.contains("gacha_status only supports"), "cmd={cmd}: {e}");
            }
        }
    }

    #[test]
    fn unknown_node_id_is_rejected() {
        let err = handle(req(Some("gacha_evil"), "status")).unwrap_err();
        assert!(err.contains("unknown gacha node_id"), "unexpected: {err}");
    }

    #[test]
    fn missing_node_id_defaults_to_entry_behavior() {
        // Backwards compat: no node_id → treat as entry (accept all).
        let r = handle(req(None, "status"));
        // Might still fail for state-file reasons, but not for
        // node_id whitelist.
        if let Err(e) = &r {
            assert!(!e.contains("only supports"), "unexpected: {e}");
        }
    }
}
