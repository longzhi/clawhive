use serde::{Deserialize, Serialize};

/// A chunk of text from a memory file, used for search results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryChunk {
    pub path: String,
    pub source: MemorySource,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MemorySource {
    LongTerm,
    Daily,
}
