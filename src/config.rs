use config::{Config as ConfigLoader, ConfigError, Environment, File};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub repo: RepoConfig,
    pub embedder: EmbedderConfig,
    pub store: StoreConfig,
    pub server: ServerConfig,
    pub index: IndexConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub path: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct EmbedderConfig {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub query_prefix: Option<String>,
    #[serde(default)]
    pub doc_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    pub provider: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub vector_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub port: String,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    pub chunk_by: String,
    pub watch: bool,
}

impl AppConfig {
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        let s = ConfigLoader::builder()
            .add_source(File::with_name(path))
            .add_source(Environment::with_prefix("CODEINDEX").separator("__"))
            .build()?;

        let mut cfg: AppConfig = s.try_deserialize()?;

        // Apply defaults
        if cfg.embedder.provider.is_empty() {
            cfg.embedder.provider = "fastembed".into();
        }
        if cfg.embedder.model.is_empty() {
            cfg.embedder.model = "nomic-ai/nomic-embed-text-v1.5".into();
        }
        if cfg.store.provider.is_empty() {
            cfg.store.provider = "edge".into();
        }
        if cfg.store.path.is_empty() {
            cfg.store.path = ".compas/edge-shard".into();
        }
        if cfg.store.vector_name.is_empty() {
            cfg.store.vector_name = "default".into();
        }
        if cfg.server.port.is_empty() {
            cfg.server.port = "3001".into();
        }
        if cfg.server.host.is_empty() {
            cfg.server.host = "127.0.0.1".into();
        }
        if cfg.index.chunk_by.is_empty() {
            cfg.index.chunk_by = "function".into();
        }

        Ok(cfg)
    }
}

/// Global registry of initialized compas repos.
/// Stored at ~/.config/compas/repos.json
#[derive(Debug, Clone, Serialize, Default)]
pub struct RepoRegistry {
    /// repo_name -> absolute_path
    pub repos: HashMap<String, String>,
}

impl<'de> serde::Deserialize<'de> for RepoRegistry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept either flat {"repo": "path"} or nested {"repos": {"repo": "path"}}
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(obj) = value.as_object() {
            if let Some(repos) = obj.get("repos").and_then(|v| v.as_object()) {
                let repos: HashMap<String, String> = repos
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect();
                return Ok(RepoRegistry { repos });
            }
            // Flat format: treat top-level keys as repo names
            let repos: HashMap<String, String> = obj
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            return Ok(RepoRegistry { repos });
        }
        Ok(RepoRegistry::default())
    }
}

impl RepoRegistry {
    pub fn load() -> Self {
        let path = Self::registry_path();
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::registry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn register(&mut self, name: &str, path: &std::path::Path) {
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.repos
            .insert(name.to_string(), abs.to_string_lossy().to_string());
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.repos.get(name)
    }

    pub fn list(&self) -> Vec<(&String, &String)> {
        self.repos.iter().collect()
    }

    fn registry_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("compas")
            .join("repos.json")
    }
}
