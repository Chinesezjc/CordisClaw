// 存图路径解析的集成回归：只验证「要不要存 / 存到哪个目录」，
// 不建目录、不发网络请求（下载分支的失败只会 eprintln，不影响收消息）。

use std::path::{Path, PathBuf};

fn temp_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "qq-media-it-{}-{}-{}",
        std::process::id(),
        tag,
        nanos
    ));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn write_config(root: &Path, body: &str) {
    std::fs::write(root.join("qq_media_archive.json"), body).expect("write media archive config");
}

#[test]
fn media_archive_defaults_to_enabled_and_all_groups() {
    let root = temp_root("defaults");
    let root_s = root.to_str().expect("utf-8 temp path");

    // 没有配置文件 → 默认开启、groups = ["*"]、dir = <data_dir>/media。
    let dir = qq::media_archive_dir(Some(root_s), "group", "123456")
        .expect("缺省配置应开启存图");
    assert_eq!(dir, root.join("media").join("123456"));

    // 只处理群消息：私聊即便群在名单里也不存。
    assert!(qq::media_archive_dir(Some(root_s), "private", "123456").is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn media_archive_follows_enabled_groups_and_dir_overrides() {
    let root = temp_root("overrides");
    let root_s = root.to_str().expect("utf-8 temp path");

    // 关掉 → 一个群都不存。
    write_config(&root, r#"{"enabled": false}"#);
    assert!(qq::media_archive_dir(Some(root_s), "group", "123456").is_none());

    // groups 精确匹配：只有 999 存。
    write_config(&root, r#"{"groups": ["999"]}"#);
    assert!(qq::media_archive_dir(Some(root_s), "group", "123456").is_none());
    let dir = qq::media_archive_dir(Some(root_s), "group", "999").expect("名单内的群要存");
    assert_eq!(dir, root.join("media").join("999"));

    // 显式 dir 优先于 <data_dir>/media。
    let pics = root.join("pics");
    write_config(
        &root,
        &format!(r#"{{"dir": "{}"}}"#, pics.to_str().expect("utf-8 dir")),
    );
    let dir = qq::media_archive_dir(Some(root_s), "group", "123456").expect("显式 dir 也要存");
    assert_eq!(dir, pics.join("123456"));

    // 坏 JSON → 回落默认（开启、全部群），配置坏了也不能拖垮收消息。
    write_config(&root, "{not json");
    let dir = qq::media_archive_dir(Some(root_s), "group", "123456")
        .expect("坏配置应回落为默认开启");
    assert_eq!(dir, root.join("media").join("123456"));

    let _ = std::fs::remove_dir_all(&root);
}
