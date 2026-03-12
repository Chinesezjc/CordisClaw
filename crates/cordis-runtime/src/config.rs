use crate::core::error::RuntimeError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub runtime: RuntimeSettings,
    #[serde(default)]
    pub kernel: KernelConfig,
    #[serde(default)]
    pub llm_api: LlmApiConfig,
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
            config.llm_api = read_yaml_file(&llm_api_path)?;
        }

        let plugin_dir = config_dir.join("plugins");
        if plugin_dir.exists() {
            let mut plugin_configs = BTreeMap::new();
            let mut entries = fs::read_dir(&plugin_dir).map_err(|e| RuntimeError::Io {
                path: plugin_dir.clone(),
                message: e.to_string(),
            })?;

            let mut paths = Vec::new();
            while let Some(entry) = entries.next() {
                let entry = entry.map_err(|e| RuntimeError::Io {
                    path: plugin_dir.clone(),
                    message: e.to_string(),
                })?;
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
    #[serde(default)]
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
        }
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
    let sibling = fixtures_root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config");
    if sibling.exists() || fixtures_root.file_name().and_then(|name| name.to_str()) == Some("fixtures") {
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
