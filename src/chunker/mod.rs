pub mod dart;
pub mod rust;

#[cfg(test)]
mod dart_test;
#[cfg(test)]
mod rust_test;

use crate::models::Chunk;
use anyhow::Result;

pub(crate) fn truncate_content(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let mut end = max_bytes.min(content.len());
    while end < content.len() && !content.is_char_boundary(end) {
        end += 1;
    }

    if let Some(newline_offset) = content[end..].find('\n') {
        end += newline_offset + 1;
    } else {
        end = content.len();
    }

    content[..end].to_string()
}

pub trait Chunker: Send + Sync {
    fn language(&self) -> &'static str;
    fn chunk(&self, file_path: &str, content: &str) -> Result<Vec<Chunk>>;
}

pub struct ChunkerRegistry {
    chunkers: Vec<Box<dyn Chunker>>,
}

impl ChunkerRegistry {
    pub fn new() -> Self {
        let mut r = Self { chunkers: vec![] };
        r.register(Box::new(dart::DartChunker));
        r.register(Box::new(rust::RustChunker));
        r
    }

    pub fn register(&mut self, c: Box<dyn Chunker>) {
        self.chunkers.push(c);
    }

    pub fn get(&self, lang: &str) -> Option<&dyn Chunker> {
        self.chunkers
            .iter()
            .find(|c| c.language() == lang)
            .map(|b| b.as_ref())
    }
}

impl Default for ChunkerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub fn language_for_path(path: &std::path::Path) -> Option<&'static str> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("dart") => Some("dart"),
        Some("rs") => Some("rust"),
        _ => None,
    }
}

pub fn extract_calls_for_language(
    language: &str,
    content: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    match language {
        "dart" => dart::extract_calls(content),
        "rust" => rust::extract_calls(content),
        _ => Ok(Vec::new()),
    }
}
