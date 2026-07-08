use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// Description of a single language server and the files it claims.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub language_id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub file_extensions: Vec<String>,
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl ServerConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.unwrap_or(60))
    }
}

/// The full registry of configured language servers.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

/// The default registry, embedded at build time.
const BUILTIN: &str = include_str!("../config/default.toml");

impl Config {
    pub fn builtin() -> Self {
        toml::from_str(BUILTIN).expect("built-in config/default.toml must parse")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Ok(toml::from_str(&text)?)
    }

    pub fn load_or_builtin(path: Option<&Path>) -> Self {
        match path {
            Some(p) => Config::load(p).unwrap_or_else(|e| {
                tracing::warn!("failed to load config {p:?}: {e:#}; falling back to built-in");
                Config::builtin()
            }),
            None => Config::builtin(),
        }
    }

    pub fn language_for_ext(&self, ext: &str) -> Option<&ServerConfig> {
        self.servers
            .iter()
            .find(|s| s.file_extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
    }

    pub fn language_for_path(&self, path: &Path) -> Option<&ServerConfig> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(|e| self.language_for_ext(e))
    }

    #[allow(dead_code)]
    pub fn language(&self, language_id: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.language_id == language_id)
    }
}
