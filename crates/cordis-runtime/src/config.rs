use crate::core::error::RuntimeError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Map a filesystem path + error into `RuntimeError::Io`, preserving the
/// error text byte-for-byte via `to_string`. Extracted so the per-entry
/// `read_dir` error mapper — which is hard to trigger deterministically in a
/// portable test (it requires a mid-iteration `DirEntry` failure) — is
/// directly unit-testable.
fn io_error(path: &Path, message: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Io {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

fn default_change_history_limit() -> usize {
    1_024
}

fn default_min_quality_score() -> u32 {
    80
}

fn default_provider() -> String {
    "openai".to_string()
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}

fn default_model() -> String {
    "gpt-4.1-mini".to_string()
}

fn default_temperature() -> f32 {
    0.2
}

fn default_max_tokens() -> u32 {
    4_096
}

fn default_timeout_ms() -> u64 {
    60_000
}

fn default_stream_timeout_secs() -> u64 {
    60
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub runtime: RuntimeSettings,
    #[serde(default)]
    pub kernel: KernelConfig,
    /// The "default" profile's API config. Kept as a direct field so the
    /// many existing `config.llm_api` call sites keep working; it is
    /// always synchronised with `llm_profiles.profiles["default"].api`.
    #[serde(default)]
    pub llm_api: LlmApiConfig,
    /// Named LLM profiles (default/fast/…). Users select by name (via
    /// soul records); credentials stay in env vars referenced by
    /// `api_key_env` — never in any per-user store.
    #[serde(default)]
    pub llm_profiles: LlmProfileRegistry,
    #[serde(default)]
    pub plugin_configs: BTreeMap<String, PluginConfigFile>,
    #[serde(skip)]
    pub config_dir: PathBuf,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            runtime: RuntimeSettings::default(),
            kernel: KernelConfig::default(),
            llm_api: LlmApiConfig::default(),
            llm_profiles: LlmProfileRegistry::default(),
            plugin_configs: BTreeMap::new(),
            config_dir: PathBuf::from("config"),
        }
    }
}

impl RuntimeConfig {
    pub fn load(fixtures_root: &Path) -> Result<Self, RuntimeError> {
        let config_dir = discover_config_dir(fixtures_root);
        let mut config = RuntimeConfig {
            config_dir: config_dir.clone(),
            ..RuntimeConfig::default()
        };

        if !config_dir.exists() {
            return Ok(config);
        }

        let runtime_path = config_dir.join("runtime.yaml");
        if runtime_path.exists() {
            let partial: RuntimeFile = read_yaml_file(&runtime_path)?;
            if let Some(runtime) = partial.runtime {
                config.runtime = runtime;
            }
            if let Some(kernel) = partial.kernel {
                config.kernel = kernel;
            }
        }

        let llm_api_path = config_dir.join("llm_api.yaml");
        if llm_api_path.exists() {
            let raw: serde_yaml::Value = read_yaml_file(&llm_api_path)?;
            config.llm_profiles = LlmProfileRegistry::from_yaml_value(raw).map_err(|message| {
                RuntimeError::ConfigParse {
                    path: llm_api_path.clone(),
                    message,
                }
            })?;
            config.llm_api = config.llm_profiles.default_profile().api.clone();
        }

        let plugin_dir = config_dir.join("plugins");
        if plugin_dir.exists() {
            let mut plugin_configs = BTreeMap::new();
            let entries = fs::read_dir(&plugin_dir).map_err(|e| RuntimeError::Io {
                path: plugin_dir.clone(),
                message: e.to_string(),
            })?;

            let mut paths = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| io_error(&plugin_dir, e))?;
                let path = entry.path();
                if !matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yaml") | Some("yml")
                ) {
                    continue;
                }
                paths.push(path);
            }
            paths.sort();

            for path in paths {
                let mut plugin_config: PluginConfigFile = read_yaml_file(&path)?;
                if plugin_config.plugin.trim().is_empty() {
                    plugin_config.plugin = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or_default()
                        .to_string();
                }
                plugin_configs.insert(plugin_config.plugin.clone(), plugin_config);
            }

            config.plugin_configs = plugin_configs;
        }

        Ok(config)
    }

    pub fn resolve_snapshot_root(&self, fixtures_root: &Path) -> Option<PathBuf> {
        let raw = self.runtime.snapshot_root.as_ref()?;
        if raw.trim().is_empty() {
            return None;
        }

        let path = Path::new(raw);
        Some(if path.is_absolute() {
            path.to_path_buf()
        } else if self.config_dir.exists() {
            self.config_dir.join(path)
        } else {
            fixtures_root.join(path)
        })
    }

    /// snapshot 目录的保留时长；未配置时返回默认 24 小时。
    /// GC（`cleanup_orphaned_snapshot_roots`）据此判断 hash 目录是否已过期。
    ///
    /// 语义约定：
    /// - `None`（键缺省）→ 24 小时。
    /// - `Some(0)` → `Duration::ZERO`，即所有目录立即过期；不回落到默认值，
    ///   因为运维在 gc 场景显式写 0 就是要让全部目录可回收。
    /// - 极大值经 `saturating_mul` 收敛到 `u64::MAX` 秒，不会溢出 panic。
    pub fn snapshot_retention(&self) -> std::time::Duration {
        const DEFAULT_RETENTION_HOURS: u64 = 24;
        let hours = self
            .runtime
            .snapshot_retention_hours
            .unwrap_or(DEFAULT_RETENTION_HOURS);
        std::time::Duration::from_secs(hours.saturating_mul(3_600))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
struct RuntimeFile {
    #[serde(default)]
    runtime: Option<RuntimeSettings>,
    #[serde(default)]
    kernel: Option<KernelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RuntimeSettings {
    #[serde(default)]
    pub snapshot_root: Option<String>,
    /// snapshot 目录的保留时长，单位小时。缺省（`None`）时使用 24 小时默认值；
    /// 显式配 `0` 表示立即过期。
    #[serde(default)]
    pub snapshot_retention_hours: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelConfig {
    #[serde(default = "default_change_history_limit")]
    pub change_history_limit: usize,
    #[serde(default = "default_min_quality_score")]
    pub min_quality_score: u32,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            change_history_limit: default_change_history_limit(),
            min_quality_score: default_min_quality_score(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmApiConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// Optional literal API key. Loaded from config file (deserialize) but
    /// P0-25: never serialized back to disk, so session auto-save /
    /// shutdown-memory snapshots don't leak the key in plaintext. Callers
    /// that need the key at request time still read `resolve_api_key` which
    /// falls back to `api_key_env`.
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_stream_timeout_secs")]
    pub stream_timeout_secs: u64,
}

impl Default for LlmApiConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            base_url: default_base_url(),
            api_key_env: default_api_key_env(),
            api_key: None,
            model: default_model(),
            organization: None,
            project: None,
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            timeout_ms: default_timeout_ms(),
            stream_timeout_secs: default_stream_timeout_secs(),
        }
    }
}

/// A named LLM configuration plus an optional fallback pointer. The
/// fallback names another profile the runtime mechanically switches to
/// when requests through this profile exhaust their retries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmProfile {
    #[serde(flatten)]
    pub api: LlmApiConfig,
    #[serde(default)]
    pub fallback: Option<String>,
}

/// Named LLM profile table parsed from `llm_api.yaml`. Two accepted
/// formats: the new `profiles: {name: {...}}` table, and the legacy
/// single-config document which is wrapped as the `default` profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmProfileRegistry {
    #[serde(default)]
    pub profiles: BTreeMap<String, LlmProfile>,
}

impl Default for LlmProfileRegistry {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "default".to_string(),
            LlmProfile {
                api: LlmApiConfig::default(),
                fallback: None,
            },
        );
        Self { profiles }
    }
}

impl LlmProfileRegistry {
    /// Parse either yaml format. Errors only on malformed profile bodies;
    /// a missing `default` entry is backfilled from `LlmApiConfig::default()`
    /// so `resolve` always has a landing spot.
    pub fn from_yaml_value(raw: serde_yaml::Value) -> Result<Self, String> {
        let mut registry = if raw.get("profiles").is_some() {
            serde_yaml::from_value::<LlmProfileRegistry>(raw).map_err(|e| e.to_string())?
        } else {
            let api = serde_yaml::from_value::<LlmApiConfig>(raw).map_err(|e| e.to_string())?;
            let mut profiles = BTreeMap::new();
            profiles.insert(
                "default".to_string(),
                LlmProfile {
                    api,
                    fallback: None,
                },
            );
            Self { profiles }
        };
        registry
            .profiles
            .entry("default".to_string())
            .or_insert_with(|| LlmProfile {
                api: LlmApiConfig::default(),
                fallback: None,
            });
        Ok(registry)
    }

    /// Look up a profile by name; unknown or empty names fall back to
    /// `default` (guaranteed present by construction).
    pub fn resolve(&self, name: &str) -> &LlmProfile {
        self.profiles
            .get(name)
            .unwrap_or_else(|| self.default_profile())
    }

    pub fn default_profile(&self) -> &LlmProfile {
        self.profiles
            .get("default")
            .expect("LlmProfileRegistry always contains a default profile")
    }

    /// The fallback target for `name`, if it declares one that exists and
    /// differs from itself (self-loops would make the switch a no-op).
    pub fn fallback_of(&self, name: &str) -> Option<&str> {
        let target = self.resolve(name).fallback.as_deref()?;
        if target == name || !self.profiles.contains_key(target) {
            return None;
        }
        Some(target)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginConfigFile {
    #[serde(default)]
    pub plugin: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub settings: Value,
}

impl Default for PluginConfigFile {
    fn default() -> Self {
        Self {
            plugin: String::new(),
            enabled: default_enabled(),
            settings: Value::Object(Default::default()),
        }
    }
}

pub fn discover_config_dir(fixtures_root: &Path) -> PathBuf {
    // Explicit override first. The sibling-directory heuristic below breaks
    // whenever fixtures are copied to a temp dir (tests, git worktrees where
    // `config/` is gitignored) — `CORDIS_CONFIG_DIR` lets those environments
    // point at a real config without symlink games.
    if let Some(dir) = std::env::var_os("CORDIS_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    let sibling = fixtures_root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config");
    if sibling.exists()
        || fixtures_root.file_name().and_then(|name| name.to_str()) == Some("fixtures")
    {
        return sibling;
    }
    fixtures_root.join("config")
}

fn read_yaml_file<T>(path: &Path) -> Result<T, RuntimeError>
where
    T: for<'de> Deserialize<'de>,
{
    let text = fs::read_to_string(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    serde_yaml::from_str(&text).map_err(|e| RuntimeError::ConfigParse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod llm_profile_tests {
    use super::*;

    // 旧格式（单份 LlmApiConfig 文档）必须被包装为 default profile。
    #[test]
    fn parse_legacy_single_profile() {
        let raw: serde_yaml::Value = serde_yaml::from_str(
            "provider: deepseek\nbase_url: https://api.deepseek.com/v1\nmodel: deepseek-chat\n",
        )
        .unwrap();
        let reg = LlmProfileRegistry::from_yaml_value(raw).unwrap();
        assert_eq!(reg.profiles.len(), 1);
        let def = reg.default_profile();
        assert_eq!(def.api.provider, "deepseek");
        assert_eq!(def.api.model, "deepseek-chat");
        assert!(def.fallback.is_none());
    }

    #[test]
    fn parse_profile_table() {
        let raw: serde_yaml::Value = serde_yaml::from_str(
            "profiles:\n  default:\n    provider: deepseek\n    model: deepseek-chat\n    fallback: fast\n  fast:\n    provider: openai\n    model: gpt-4o-mini\n",
        )
        .unwrap();
        let reg = LlmProfileRegistry::from_yaml_value(raw).unwrap();
        assert_eq!(reg.profiles.len(), 2);
        assert_eq!(reg.resolve("fast").api.model, "gpt-4o-mini");
        assert_eq!(reg.fallback_of("default"), Some("fast"));
        assert_eq!(reg.fallback_of("fast"), None);
    }

    #[test]
    fn resolve_unknown_falls_back_default() {
        let reg = LlmProfileRegistry::default();
        assert_eq!(
            reg.resolve("nonexistent").api.provider,
            reg.default_profile().api.provider
        );
    }

    // profiles 表缺 default 时自动补齐；fallback 指向不存在/自身时失效。
    #[test]
    fn missing_default_backfilled_and_bad_fallbacks_ignored() {
        let raw: serde_yaml::Value = serde_yaml::from_str(
            "profiles:\n  fast:\n    provider: openai\n    model: gpt-4o-mini\n    fallback: fast\n",
        )
        .unwrap();
        let reg = LlmProfileRegistry::from_yaml_value(raw).unwrap();
        assert!(reg.profiles.contains_key("default"), "default 自动补齐");
        assert_eq!(reg.fallback_of("fast"), None, "自指 fallback 无效");
        let raw: serde_yaml::Value = serde_yaml::from_str(
            "profiles:\n  default:\n    provider: openai\n    fallback: ghost\n",
        )
        .unwrap();
        let reg = LlmProfileRegistry::from_yaml_value(raw).unwrap();
        assert_eq!(
            reg.fallback_of("default"),
            None,
            "不存在的 fallback 目标无效"
        );
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    /// Point every test at its own config dir via `CORDIS_CONFIG_DIR`,
    /// avoiding the sibling-directory heuristic and cross-test env leakage.
    /// Serialized because it mutates a process-global env var.
    struct ConfigDirGuard;
    impl ConfigDirGuard {
        fn set(dir: &Path) -> Self {
            std::env::set_var("CORDIS_CONFIG_DIR", dir);
            ConfigDirGuard
        }
    }
    impl Drop for ConfigDirGuard {
        fn drop(&mut self) {
            std::env::remove_var("CORDIS_CONFIG_DIR");
        }
    }

    // ---------- io_error mapper ----------

    // Equivalent logic of the per-entry read_dir error mapper (config.rs
    // 133-135): a mid-iteration DirEntry failure is not portably reproducible,
    // so the extracted mapper is asserted directly — path + text byte-for-byte.
    #[test]
    fn io_error_preserves_path_and_message() {
        let err = io_error(Path::new("/plugins"), "permission denied");
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == &PathBuf::from("/plugins") && message == "permission denied"),
            "expected Io, got {err:?}"
        );
    }

    // ---------- discover_config_dir ----------

    #[test]
    #[serial]
    fn discover_honors_env_override() {
        let tmp = TempDir::new().unwrap();
        let _g = ConfigDirGuard::set(tmp.path());
        assert_eq!(discover_config_dir(Path::new("/anything")), tmp.path());
    }

    #[test]
    #[serial]
    fn discover_empty_env_is_ignored() {
        // An empty override must not short-circuit the heuristic.
        std::env::set_var("CORDIS_CONFIG_DIR", "");
        let out = discover_config_dir(Path::new("/tmp/whatever/fixtures"));
        std::env::remove_var("CORDIS_CONFIG_DIR");
        // fixtures dir name triggers the sibling `config` branch.
        assert_eq!(out, Path::new("/tmp/whatever/config"));
    }

    #[test]
    #[serial]
    fn discover_fixtures_named_dir_uses_sibling() {
        std::env::remove_var("CORDIS_CONFIG_DIR");
        let out = discover_config_dir(Path::new("/root/fixtures"));
        assert_eq!(out, Path::new("/root/config"));
    }

    #[test]
    #[serial]
    fn discover_non_fixtures_falls_back_to_join() {
        std::env::remove_var("CORDIS_CONFIG_DIR");
        // A non-"fixtures" dir with no sibling config/ joins config under it.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir_all(&root).unwrap();
        assert_eq!(discover_config_dir(&root), root.join("config"));
    }

    // ---------- RuntimeConfig::load ----------

    #[test]
    #[serial]
    fn load_missing_config_dir_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("no-config-here");
        let _g = ConfigDirGuard::set(&missing);
        let cfg = RuntimeConfig::load(tmp.path()).unwrap();
        // Defaults intact when the config dir does not exist.
        assert_eq!(cfg.kernel.change_history_limit, 1_024);
        assert_eq!(cfg.kernel.min_quality_score, 80);
        assert_eq!(cfg.llm_api.provider, "openai");
        assert!(cfg.plugin_configs.is_empty());
        assert_eq!(cfg.config_dir, missing);
    }

    #[test]
    #[serial]
    fn load_runtime_yaml_overrides_kernel_and_runtime() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("runtime.yaml"),
            "runtime:\n  snapshot_root: snaps\n  snapshot_retention_hours: 6\nkernel:\n  change_history_limit: 7\n  min_quality_score: 55\n",
        )
        .unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let cfg = RuntimeConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.kernel.change_history_limit, 7);
        assert_eq!(cfg.kernel.min_quality_score, 55);
        assert_eq!(cfg.runtime.snapshot_root.as_deref(), Some("snaps"));
        // `RuntimeFile.runtime` 整体反序列化 `RuntimeSettings`，新键无需额外解析逻辑。
        assert_eq!(cfg.runtime.snapshot_retention_hours, Some(6));
        assert_eq!(
            cfg.snapshot_retention(),
            std::time::Duration::from_secs(6 * 3_600)
        );
    }

    #[test]
    #[serial]
    fn load_runtime_yaml_partial_keeps_defaults() {
        // Only `runtime` present; `kernel` absent → kernel stays default.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("runtime.yaml"),
            "runtime:\n  snapshot_root: /abs/snaps\n",
        )
        .unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let cfg = RuntimeConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.kernel.change_history_limit, 1_024);
        assert_eq!(cfg.runtime.snapshot_root.as_deref(), Some("/abs/snaps"));
    }

    #[test]
    #[serial]
    fn load_bad_runtime_yaml_is_config_parse_error() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("runtime.yaml"),
            "kernel:\n  change_history_limit: \"not a number\"\n",
        )
        .unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let err = RuntimeConfig::load(dir.path()).unwrap_err();
        assert!(matches!(err, RuntimeError::ConfigParse { .. }));
    }

    #[test]
    #[serial]
    fn load_llm_api_yaml_legacy_and_profiles() {
        // Legacy single-doc form becomes the default profile + syncs llm_api.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("llm_api.yaml"),
            "provider: deepseek\nmodel: deepseek-chat\n",
        )
        .unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let cfg = RuntimeConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.llm_api.provider, "deepseek");
        assert_eq!(
            cfg.llm_profiles.default_profile().api.model,
            "deepseek-chat"
        );
    }

    #[test]
    #[serial]
    fn load_bad_llm_api_yaml_is_config_parse_error() {
        let dir = TempDir::new().unwrap();
        // `profiles` present but a body is a scalar, not a profile map.
        fs::write(
            dir.path().join("llm_api.yaml"),
            "profiles:\n  default: 12345\n",
        )
        .unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let err = RuntimeConfig::load(dir.path()).unwrap_err();
        assert!(matches!(err, RuntimeError::ConfigParse { .. }));
    }

    #[test]
    #[serial]
    fn load_scans_plugin_dir_and_backfills_name() {
        let dir = TempDir::new().unwrap();
        let plugins = dir.path().join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        // Explicit plugin name.
        fs::write(
            plugins.join("a.yaml"),
            "plugin: alpha\nenabled: false\nsettings:\n  k: v\n",
        )
        .unwrap();
        // Empty plugin field → backfilled from file stem "beta".
        fs::write(plugins.join("beta.yml"), "settings: {}\n").unwrap();
        // Non-yaml file is ignored.
        fs::write(plugins.join("notes.txt"), "ignore me\n").unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let cfg = RuntimeConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.plugin_configs.len(), 2);
        let alpha = &cfg.plugin_configs["alpha"];
        assert!(!alpha.enabled);
        assert_eq!(alpha.settings["k"], "v");
        // beta backfilled its name and defaults enabled=true.
        let beta = &cfg.plugin_configs["beta"];
        assert!(beta.enabled);
    }

    #[test]
    #[serial]
    fn load_bad_plugin_yaml_is_config_parse_error() {
        let dir = TempDir::new().unwrap();
        let plugins = dir.path().join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        fs::write(plugins.join("broken.yaml"), "enabled: \"yes please\"\n").unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let err = RuntimeConfig::load(dir.path()).unwrap_err();
        assert!(matches!(err, RuntimeError::ConfigParse { .. }));
    }

    // `plugins` exists but is a FILE, not a directory: the `plugin_dir.exists()`
    // guard passes, but `fs::read_dir` then fails with a non-parse Io error.
    #[test]
    #[serial]
    fn load_plugins_path_is_file_is_io_error() {
        let dir = TempDir::new().unwrap();
        // A regular file named `plugins` where a directory is expected.
        fs::write(dir.path().join("plugins"), b"not a dir").unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let err = RuntimeConfig::load(dir.path()).unwrap_err();
        assert!(matches!(err, RuntimeError::Io { .. }), "got {err:?}");
    }

    // `runtime.yaml` exists but is a DIRECTORY: `read_yaml_file`'s
    // `read_to_string` fails with a non-parse Io error (exercises the Io
    // arm rather than ConfigParse).
    #[test]
    #[serial]
    fn load_runtime_yaml_path_is_directory_is_io_error() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("runtime.yaml")).unwrap();
        let _g = ConfigDirGuard::set(dir.path());
        let err = RuntimeConfig::load(dir.path()).unwrap_err();
        assert!(matches!(err, RuntimeError::Io { .. }), "got {err:?}");
    }

    // ---------- resolve_snapshot_root ----------

    #[test]
    fn resolve_snapshot_root_none_when_unset_or_blank() {
        let mut cfg = RuntimeConfig::default();
        assert!(cfg.resolve_snapshot_root(Path::new("/fx")).is_none());
        cfg.runtime.snapshot_root = Some("   ".to_string());
        assert!(cfg.resolve_snapshot_root(Path::new("/fx")).is_none());
    }

    #[test]
    fn resolve_snapshot_root_absolute_is_returned_verbatim() {
        let mut cfg = RuntimeConfig::default();
        cfg.runtime.snapshot_root = Some("/abs/snaps".to_string());
        assert_eq!(
            cfg.resolve_snapshot_root(Path::new("/fx")).unwrap(),
            PathBuf::from("/abs/snaps")
        );
    }

    #[test]
    fn resolve_snapshot_root_relative_joins_config_dir_when_present() {
        let dir = TempDir::new().unwrap();
        let mut cfg = RuntimeConfig {
            config_dir: dir.path().to_path_buf(),
            ..RuntimeConfig::default()
        };
        cfg.runtime.snapshot_root = Some("rel".to_string());
        assert_eq!(
            cfg.resolve_snapshot_root(Path::new("/fx")).unwrap(),
            dir.path().join("rel")
        );
    }

    #[test]
    fn resolve_snapshot_root_relative_joins_fixtures_when_no_config_dir() {
        let mut cfg = RuntimeConfig {
            config_dir: PathBuf::from("/does/not/exist"),
            ..RuntimeConfig::default()
        };
        cfg.runtime.snapshot_root = Some("rel".to_string());
        assert_eq!(
            cfg.resolve_snapshot_root(Path::new("/fx")).unwrap(),
            PathBuf::from("/fx/rel")
        );
    }

    // ---------- snapshot_retention ----------

    #[test]
    fn snapshot_retention_defaults_to_24h() {
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.runtime.snapshot_retention_hours, None);
        assert_eq!(
            cfg.snapshot_retention(),
            std::time::Duration::from_secs(24 * 3_600)
        );
    }

    #[test]
    fn snapshot_retention_honours_explicit_value() {
        let mut cfg = RuntimeConfig::default();
        cfg.runtime.snapshot_retention_hours = Some(1);
        assert_eq!(
            cfg.snapshot_retention(),
            std::time::Duration::from_secs(3_600)
        );
    }

    // 显式 0 表示立即过期，不能回落到 24 小时默认值。
    #[test]
    fn snapshot_retention_zero_expires_immediately() {
        let mut cfg = RuntimeConfig::default();
        cfg.runtime.snapshot_retention_hours = Some(0);
        assert_eq!(cfg.snapshot_retention(), std::time::Duration::ZERO);
    }

    // hours * 3600 会溢出 u64，saturating 后收敛到 u64::MAX 秒而非 panic。
    #[test]
    fn snapshot_retention_saturates_on_overflow() {
        let mut cfg = RuntimeConfig::default();
        cfg.runtime.snapshot_retention_hours = Some(u64::MAX);
        assert_eq!(
            cfg.snapshot_retention(),
            std::time::Duration::from_secs(u64::MAX)
        );
    }

    // ---------- PluginConfigFile / defaults ----------

    #[test]
    fn plugin_config_file_default_enabled_and_empty_object() {
        let pc = PluginConfigFile::default();
        assert!(pc.enabled);
        assert!(pc.plugin.is_empty());
        assert!(pc.settings.is_object());
    }
}
