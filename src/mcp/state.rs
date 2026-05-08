use crate::embedder::Embedder;
use crate::graph::Graph;
use crate::store::Store;
use std::collections::HashMap;
use std::sync::Arc;

pub struct RepoState {
    pub store: Arc<dyn Store>,
    pub graph: Arc<Graph>,
    pub embedder: Arc<dyn Embedder>,
}

pub struct McpAppState {
    pub repos: HashMap<String, RepoState>,
    pub default_repo: Option<String>,
}
