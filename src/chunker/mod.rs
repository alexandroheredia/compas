pub mod dart;

#[cfg(test)]
mod dart_test;

use crate::models::Chunk;
use anyhow::Result;

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
