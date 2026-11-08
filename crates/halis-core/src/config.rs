use crate::error::{CoreError, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    #[default]
    Confirm,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub kind: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    pub approval: ApprovalMode,
    pub max_turns: Option<u32>,
    pub effort: Option<Effort>,
    pub bash_timeout_secs: Option<u64>,
    pub providers: HashMap<String, ProviderConfig>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
        let cfg: Config = toml::from_str(&raw)
            .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
        Ok(cfg)
    }

    pub fn path() -> Result<PathBuf> {
        if let Ok(p) = std::env::var("HALIS_CONFIG") {
            return Ok(PathBuf::from(p));
        }
        let dir = dirs::config_dir()
            .ok_or_else(|| CoreError::Config("cannot determine the config directory".into()))?;
        Ok(dir.join("halis").join("config.toml"))
    }

    pub fn data_dir() -> Result<PathBuf> {
        if let Ok(p) = std::env::var("HALIS_DATA") {
            return Ok(PathBuf::from(p));
        }
        let dir = dirs::data_dir()
            .ok_or_else(|| CoreError::Config("cannot determine the data directory".into()))?;
        Ok(dir.join("halis"))
    }

    pub fn sessions_dir() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("sessions"))
    }
}
