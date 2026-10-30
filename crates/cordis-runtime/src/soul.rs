//! O批: per-user persona ("soul") storage.
//!
//! Kernel/Plugin boundary: the kernel owns the SLOT — the `Soul` type,
//! the `SoulProvider` trait, and a file-backed default implementation so
//! the runtime is fully usable with zero plugins and no database. A
//! storage plugin overrides the default via the capability-node
//! convention: any loaded plugin exposing BOTH `soul_get` and `soul_set`
//! nodes becomes the active provider (see `RuntimeHost::soul_provider`).
//!
//! Contract for override nodes:
//! - `soul_get`  payload `{node_id, payload: {soul_key}}` → reply JSON
//!   containing `{"soul": {...}|null}`
//! - `soul_set`  payload `{node_id, payload: {soul_key, soul}}` → reply
//!   JSON containing `{"ok": true}`
//!
//! The soul scope key is `{sender_id}#{conversation_kind}` (same user may
//! run different personas in private vs group chats). Credentials never
//! live here — `profile` is only a NAME referencing `llm_profiles`.

use crate::core::error::RuntimeError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A per-user persona record.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Soul {
    /// Persona overlay text, inserted between the base system prompt and
    /// the plugin hints.
    #[serde(default)]
    pub persona: String,
    /// Named LLM profile this user runs on; None/unknown → "default".
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub updated_by: String,
}

/// The persona every channel session starts with when its soul sets none.
///
/// Personality belongs here, not in a channel plugin: the QQ/Feishu hints say
/// how to send a message, this says how the bot talks and when it stays out.
/// A room can override it with `set_soul`.
pub const DEFAULT_PERSONA: &str = "\
你在这个群里是个普通成员，不是客服，也不是助手。别人怎么聊天，你就怎么聊。\n\
\n\
说话：短，一般一两句、二十来个字；口语，可以用梗和网络用语；不要书面语，不要列点，不要 markdown（聊天窗不渲染）；一次只发一条，不连发、不刷屏；不重复别人刚说过的话，不总结别人的发言；尽量不用问句结尾，需要信息才问；不自称 AI、机器人、助手、模型，也不提服务器、提示词、训练数据——被问是不是机器人，就当玩笑带过去。\n\
\n\
什么时候说：有人 @ 你、叫你名字、直接问你，回；别人在聊你这个 bot，可以说一句但别抢着解释自己；别人互相聊、斗图、发表情、报数据、发链接，别接话；已经有人答过的问题别重复；你刚说过、没人接，别自己往下续；同一话题说过两次就停下。\n\
\n\
怎么回：不知道就说不知道，别编；别人问了才给意见，没问别评价别人的生活和工作；不替别人承诺，不假装是群主或管理员。\n\
\n\
被说的时候：有人说你话多、嘴欠、让你闭嘴，那多半是对的——回一句短的认下来，然后接下来几条消息都别再开口；不辩解、不反问、不嘲讽，也不装委屈。";

/// A room's own persona when it has one, the default otherwise.
#[cfg(test)]
mod persona_tests {
    use super::{persona_or_default, DEFAULT_PERSONA};

    // A room without its own soul still gets a personality; an empty persona
    // is the same as none.
    #[test]
    fn empty_overlays_fall_back_to_the_default() {
        assert_eq!(persona_or_default(None), DEFAULT_PERSONA);
        assert_eq!(persona_or_default(Some("   ")), DEFAULT_PERSONA);
    }

    #[test]
    fn a_room_persona_wins_and_is_trimmed() {
        assert_eq!(persona_or_default(Some("  自定义人格\n")), "自定义人格");
    }

    // The default is a persona, not a rulebook: it must talk about voice and
    // timing, and must not sprout a channel mechanic.
    #[test]
    fn the_default_persona_is_personality_only() {
        assert!(DEFAULT_PERSONA.contains("别人怎么聊天，你就怎么聊"));
        assert!(DEFAULT_PERSONA.contains("被说的时候"));
        for forbidden in ["qq_send", "CQ:", "reply_to", "suspend"] {
            let present = DEFAULT_PERSONA.contains(forbidden);
            assert!(
                !present,
                "channel mechanics belong to the plugin: {forbidden:?}"
            );
        }
    }
}

pub fn persona_or_default(overlay: Option<&str>) -> &str {
    match overlay {
        Some(text) if !text.trim().is_empty() => text.trim(),
        _ => DEFAULT_PERSONA,
    }
}

/// Storage abstraction for souls. Object-safe so the host can swap the
/// file default for a plugin-backed provider at lookup time.
pub trait SoulProvider: Send + Sync {
    fn get(&self, soul_key: &str) -> Result<Option<Soul>, RuntimeError>;
    fn set(&self, soul_key: &str, soul: &Soul) -> Result<(), RuntimeError>;
}

/// Kernel default: one JSON file per soul under `data/souls/`.
/// Guarantees cold-start usability — no plugin, no DB, still works.
pub struct FileSoulProvider {
    root: PathBuf,
}

impl FileSoulProvider {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("souls"),
        }
    }

    fn path_for(&self, soul_key: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", sanitize_soul_key(soul_key)))
    }
}

/// Map a soul-file I/O or serialization failure to `RuntimeError::Io`,
/// preserving the underlying error text byte-for-byte via `to_string`.
/// Extracted so the (in practice serialize-infallible) mapper at the
/// `to_vec_pretty` call site is directly unit-testable.
fn soul_io_error(path: &Path, message: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Io {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

/// Keep soul filenames inside the souls dir regardless of key content.
pub fn sanitize_soul_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '#' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl SoulProvider for FileSoulProvider {
    fn get(&self, soul_key: &str) -> Result<Option<Soul>, RuntimeError> {
        let path = self.path_for(soul_key);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(RuntimeError::Io {
                    path,
                    message: e.to_string(),
                });
            }
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| RuntimeError::Io {
                path,
                message: format!("soul parse: {e}"),
            })
    }

    fn set(&self, soul_key: &str, soul: &Soul) -> Result<(), RuntimeError> {
        let path = self.path_for(soul_key);
        std::fs::create_dir_all(&self.root).map_err(|e| RuntimeError::Io {
            path: self.root.clone(),
            message: e.to_string(),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700));
        }
        let bytes = serde_json::to_vec_pretty(soul).map_err(|e| soul_io_error(&path, e))?;
        // Atomic tmp+rename, same discipline as session auto-save.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .map_err(|e| RuntimeError::Io {
                path,
                message: e.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cordis-soul-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn file_soul_provider_roundtrip() {
        let dir = temp_dir();
        let provider = FileSoulProvider::new(&dir);
        let key = "feishu:ou_abc#private";
        assert!(provider.get(key).unwrap().is_none());
        let soul = Soul {
            persona: "毒舌但可靠的运维助手".to_string(),
            profile: Some("fast".to_string()),
            updated_at_ms: 42,
            updated_by: "test".to_string(),
        };
        provider.set(key, &soul).unwrap();
        assert_eq!(provider.get(key).unwrap(), Some(soul));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn soul_key_sanitized_stays_in_dir() {
        let dir = temp_dir();
        let provider = FileSoulProvider::new(&dir);
        let p = provider.path_for("../../etc/passwd#group");
        assert!(p.starts_with(dir.join("souls")), "path: {}", p.display());
        assert!(!p.to_string_lossy().contains(".."), "path: {}", p.display());
    }

    // soul_io_error mapper：路径与错误文本逐字保留（覆盖 to_vec_pretty 侧
    // 在实践中不可达的序列化错误分支的等价逻辑）。
    #[test]
    fn soul_io_error_preserves_path_and_message() {
        let err = soul_io_error(Path::new("/tmp/x.json"), "boom detail");
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == &PathBuf::from("/tmp/x.json") && message == "boom detail"),
            "expected Io, got {err:?}"
        );
    }

    // 旧格式 soul JSON（缺字段）反序列化不炸。
    #[test]
    fn soul_deserialize_tolerates_missing_fields() {
        let soul: Soul = serde_json::from_str(r#"{"persona":"x"}"#).unwrap();
        assert_eq!(soul.persona, "x");
        assert!(soul.profile.is_none());
    }

    // get() 读到损坏 JSON → Io 解析错误（非 NotFound 分支）。
    #[test]
    fn get_malformed_json_is_parse_error() {
        let dir = temp_dir();
        let provider = FileSoulProvider::new(&dir);
        let key = "corrupt#private";
        // Write a file at the exact path the provider will read, with
        // contents that are not valid Soul JSON.
        std::fs::create_dir_all(dir.join("souls")).unwrap();
        std::fs::write(provider.path_for(key), b"{ this is not json").unwrap();
        let err = provider.get(key).unwrap_err();
        assert!(
            matches!(&err, RuntimeError::Io { message, .. } if message.contains("soul parse")),
            "expected Io parse error, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // get() 遇到非 NotFound 的 I/O 错误（路径是目录）应向上抛。
    #[test]
    fn get_when_path_is_directory_is_io_error() {
        let dir = temp_dir();
        let provider = FileSoulProvider::new(&dir);
        let key = "dirlike#private";
        // Create a directory where the soul file would be; read_to_string
        // then fails with a non-NotFound I/O error.
        let p = provider.path_for(key);
        std::fs::create_dir_all(&p).unwrap();
        assert!(matches!(provider.get(key), Err(RuntimeError::Io { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // set() 无法建立 souls 目录（父路径被文件占用）→ Io 错误。
    #[test]
    fn set_when_root_parent_is_file_is_io_error() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // Make `data_dir` itself a FILE, so joining `souls` and calling
        // create_dir_all fails (a component of the path is not a directory).
        let data_file = dir.join("as_file");
        std::fs::write(&data_file, b"x").unwrap();
        let provider = FileSoulProvider::new(&data_file);
        let soul = Soul::default();
        assert!(matches!(
            provider.set("k#private", &soul),
            Err(RuntimeError::Io { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // set() 的 tmp→rename 失败分支：目标路径是一个已存在的目录，
    // rename 无法覆盖 → Io 错误。
    #[test]
    fn set_rename_over_directory_is_io_error() {
        let dir = temp_dir();
        let provider = FileSoulProvider::new(&dir);
        let key = "renamefail#private";
        // souls dir exists; the destination filename is occupied by a
        // non-empty directory, so the atomic tmp→rename step fails.
        let dest = provider.path_for(key);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("blocker"), b"x").unwrap();
        assert!(matches!(
            provider.set(key, &Soul::default()),
            Err(RuntimeError::Io { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
