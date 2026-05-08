use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub content: String,
    pub language: String,
    pub file_path: String,
    pub symbol: String,
    #[serde(rename = "line_start")]
    pub line_start: usize,
    #[serde(rename = "line_end")]
    pub line_end: usize,
    #[serde(rename = "type")]
    pub kind: String,
    pub meta: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub chunk: Chunk,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolNode {
    pub name: String,
    pub file: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub calls: Vec<String>,
    #[serde(rename = "called_by")]
    pub called_by: Vec<String>,
}
