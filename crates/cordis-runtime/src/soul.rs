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
        Self { root: data_dir.join("souls") }
    }

    fn path_for(&self, soul_key: &str) -> PathBuf {
        self.root.join(format!("{}.json", sanitize_soul_key(soul_key)))
    }
}

/// Keep soul filenames inside the souls dir regardless of key content.
pub fn sanitize_soul_key(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '#' { c } else { '_' })
        .collect()
}

impl SoulProvider for FileSoulProvider {
    fn get(&self, soul_key: &str) -> Result<Option<Soul>, RuntimeError> {
        let path = self.path_for(soul_key);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(RuntimeError::Io { path, message: e.to_string() });
            }
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| RuntimeError::Io { path, message: format!("soul parse: {e}") })
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
        let bytes = serde_json::to_vec_pretty(soul).map_err(|e| RuntimeError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
        // Atomic tmp+rename, same discipline as session auto-save.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .map_err(|e| RuntimeError::Io { path, message: e.to_string() })
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

    // 旧格式 soul JSON（缺字段）反序列化不炸。
    #[test]
    fn soul_deserialize_tolerates_missing_fields() {
        let soul: Soul = serde_json::from_str(r#"{"persona":"x"}"#).unwrap();
        assert_eq!(soul.persona, "x");
        assert!(soul.profile.is_none());
    }
}
