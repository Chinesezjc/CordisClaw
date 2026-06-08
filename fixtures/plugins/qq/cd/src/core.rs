use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CdError {
    #[error("not configured")]
    NotConfigured,
    #[error("deploy failed: {0}")]
    DeployFailed(String),
    #[error("invalid action: {0}")]
    InvalidAction(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdConfig {
    pub target_url: Option<String>,
    pub api_key: Option<String>,
    pub project: Option<String>,
    pub last_deploy: Option<String>,
}

#[derive(Debug)]
pub struct CdPlugin {
    pub config: Mutex<CdConfig>,
}

impl CdPlugin {
    pub fn new() -> Self {
        CdPlugin {
            config: Mutex::new(CdConfig {
                target_url: None,
                api_key: None,
                project: None,
                last_deploy: None,
            }),
        }
    }

    pub fn configure(&self, url: &str, key: &str, project: &str) -> Result<(), CdError> {
        let mut cfg = self.config.lock().unwrap();
        cfg.target_url = Some(url.to_string());
        cfg.api_key = Some(key.to_string());
        cfg.project = Some(project.to_string());
        Ok(())
    }

    pub fn deploy(&self) -> Result<String, CdError> {
        let project;
        let timestamp;
        {
            let cfg = self.config.lock().unwrap();
            // Check configuration
            cfg.target_url.as_ref().ok_or(CdError::NotConfigured)?;
            project = cfg.project.clone().ok_or(CdError::NotConfigured)?;
            timestamp = chrono::Utc::now().to_rfc3339();
        }
        // Update last deploy timestamp
        {
            let mut cfg = self.config.lock().unwrap();
            cfg.last_deploy = Some(timestamp.clone());
        }
        Ok(format!("deployed {} at {}", project, timestamp))
    }

    pub fn status(&self) -> CdConfig {
        self.config.lock().unwrap().clone()
    }
}

// Global singleton for the plugin
static INSTANCE: std::sync::LazyLock<CdPlugin> = std::sync::LazyLock::new(|| CdPlugin::new());

pub fn get_instance() -> &'static CdPlugin {
    &INSTANCE
}
