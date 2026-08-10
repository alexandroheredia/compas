use crate::config::{AppConfig, EmbedderConfig};
use crate::embedder::{build_embedder, Embedder};
use crate::graph::Graph;
use crate::store::{edge::EdgeStore, Store};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Clone)]
pub struct RepoState {
    pub store: Arc<dyn Store>,
    pub graph: Arc<Graph>,
    pub embedder: Arc<dyn Embedder>,
}

pub struct McpAppState {
    /// In-memory map of loaded repos. Wrapped in RwLock so repos registered
    /// after the MCP server started (via `compas init` in another repo) can be
    /// loaded on demand the first time an agent queries them.
    pub repos: Arc<RwLock<HashMap<String, RepoState>>>,
    pub default_repo: Option<String>,
    /// Share embedder instances across repos with identical embedder configs,
    /// including repos loaded lazily after startup.
    pub embedder_cache: Arc<Mutex<HashMap<EmbedderConfig, Arc<dyn Embedder>>>>,
}

/// Load a single repo's state (config, embedder, store, graph) from disk.
///
/// Returns `Ok(None)` if the repo's `compas.yaml` is missing. Returns an error
/// if the config fails to load. Used both at MCP startup and on lazy reload
/// when an agent queries a repo that wasn't registered when the server started.
pub fn load_repo_state(
    name: &str,
    path: &str,
    embedder_cache: &Arc<Mutex<HashMap<EmbedderConfig, Arc<dyn Embedder>>>>,
) -> anyhow::Result<Option<RepoState>> {
    let config_path = std::path::Path::new(path).join("compas.yaml");
    if !config_path.exists() {
        tracing::warn!(
            "compas.yaml not found for repo '{}' at {}, skipping",
            name,
            path
        );
        return Ok(None);
    }

    let config = AppConfig::load(config_path.to_str().unwrap())?;
    let repo_path = std::fs::canonicalize(path)?;
    let embedder = {
        let mut cache = embedder_cache.lock().unwrap();
        match cache.get(&config.embedder) {
            Some(e) => Arc::clone(e),
            None => {
                let e = build_embedder(&config.embedder)?;
                cache.insert(config.embedder.clone(), Arc::clone(&e));
                e
            }
        }
    };
    let store: Arc<dyn Store> = Arc::new(EdgeStore::new(
        repo_path.join(&config.store.path),
        &config.store.vector_name,
    ));
    let graph = Arc::new(Graph::new());
    let graph_path = repo_path.join(".compas").join("graph.json");
    if let Err(e) = graph.load(&graph_path) {
        tracing::warn!("no existing graph loaded for repo '{}': {}", name, e);
    }

    Ok(Some(RepoState {
        store,
        graph,
        embedder,
    }))
}
