//! Block streaming chunker for progressive output.
//!
//! Splits long assistant responses into smaller chunks that can be
//! sent progressively to the user, improving perceived latency.

use std::time::{Duration, Instant};

/// Configuration for block streaming.
#[derive(Debug, Clone)]
pub struct BlockStreamingConfig {
    /// Minimum characters before emitting a chunk
    pub min_chars: usize,
    /// Maximum characters per chunk
    pub max_chars: usize,
    /// Preferred break points (in order of preference)
    pub break_preference: BreakPreference,
    /// Idle timeout before flushing partial buffer
    pub idle_ms: u64,
    /// Whether block streaming is enabled
    pub enabled: bool,
}

impl Default for BlockStreamingConfig {
    fn default() -> Self {
        Self {
            min_chars: 200,
            max_chars: 2000,
            break_preference: BreakPreference::Paragraph,
            idle_ms: 500,
            enabled: false,
        }
    }
}

/// Break preference for chunking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakPreference {
    Paragraph,
    Newline,
    Sentence,
    Whitespace,
}

impl BreakPreference {
    pub fn joiner(&self) -> &'static str {
        match self {
            Self::Paragraph => "\n\n",
            Self::Newline => "\n",
            Self::Sentence | Self::Whitespace => " ",
        }
    }
}

/// Coalescing configuration for merging chunks before sending.
#[derive(Debug, Clone)]
pub struct CoalesceConfig {
    /// Minimum characters to accumulate before sending
    pub min_chars: usize,
    /// Maximum characters before forcing a send
    pub max_chars: usize,
    /// Idle timeout before flushing
    pub idle_ms: u64,
}

impl Default for CoalesceConfig {
    fn default() -> Self {
        Self {
            min_chars: 100,
            max_chars: 4000,
            idle_ms: 300,
        }
    }
}

/// A chunk ready to be sent.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    pub text: String,
    pub is_final: bool,
}

/// Block chunker for progressive output.
pub struct BlockChunker {
    config: BlockStreamingConfig,
    buffer: String,
    last_emit: Instant,
    in_code_fence: bool,
    code_fence_marker: Option<String>,
}

impl BlockChunker {
    pub fn new(config: BlockStreamingConfig) -> Self {
        Self {
            config,
            buffer: String::new(),
            last_emit: Instant::now(),
            in_code_fence: false,
            code_fence_marker: None,
        }
    }

    /// Add text to the buffer, returning any chunks ready to emit.
    pub fn push(&mut self, text: &str) -> Vec<StreamChunk> {
        self.buffer.push_str(text);
        self.track_code_fences(text);

        let mut chunks = Vec::new();

        while self.buffer.len() >= self.config.min_chars {
            if let Some(chunk) = self.try_emit_chunk() {
                chunks.push(chunk);
                self.last_emit = Instant::now();
            } else {
                break;
            }
        }

        chunks
    }

    /// Check if we should emit due to idle timeout.
    pub fn should_flush_idle(&self) -> bool {
        !self.buffer.is_empty()
            && self.last_emit.elapsed() > Duration::from_millis(self.config.idle_ms)
    }

    /// Flush remaining buffer, returning final chunk.
    pub fn flush(mut self) -> Option<StreamChunk> {
        if self.buffer.is_empty() {
            return None;
        }

        // Close any open code fence
        let mut text = std::mem::take(&mut self.buffer);
        if self.in_code_fence {
            if let Some(marker) = &self.code_fence_marker {
                text.push_str("\n");
                text.push_str(marker);
            }
        }

        Some(StreamChunk {
            text,
            is_final: true,
        })
    }

    /// Try to emit a chunk from the buffer.
    fn try_emit_chunk(&mut self) -> Option<StreamChunk> {
        if self.buffer.len() < self.config.min_chars {
            return None;
        }

        // Don't break inside code fences unless forced by max_chars
        if self.in_code_fence && self.buffer.len() < self.config.max_chars {
            return None;
        }

        // Find best break point
        let break_point = self.find_break_point();
        if break_point == 0 {
            return None;
        }

        let mut chunk_text = self.buffer[..break_point].to_string();
        self.buffer = self.buffer[break_point..].trim_start().to_string();

        // Handle code fence split
        if self.in_code_fence && !self.buffer.is_empty() {
            // Close the fence in this chunk
            if let Some(marker) = &self.code_fence_marker {
                chunk_text.push_str("\n");
                chunk_text.push_str(marker);
            }
            // Reopen in the remaining buffer
            if let Some(marker) = &self.code_fence_marker {
                self.buffer = format!("{}\n{}", marker, self.buffer);
            }
        }

        Some(StreamChunk {
            text: chunk_text,
            is_final: false,
        })
    }

    /// Find the best break point in the buffer.
    fn find_break_point(&self) -> usize {
        let max_pos = self.buffer.len().min(self.config.max_chars);

        // Try break preferences in order
        match self.config.break_preference {
            BreakPreference::Paragraph => {
                if let Some(pos) = self.find_break_at("\n\n", max_pos) {
                    return pos;
                }
                if let Some(pos) = self.find_break_at("\n", max_pos) {
                    return pos;
                }
            }
            BreakPreference::Newline => {
                if let Some(pos) = self.find_break_at("\n", max_pos) {
                    return pos;
                }
            }
            BreakPreference::Sentence => {
                for pattern in [". ", "? ", "! ", "。", "？", "！"] {
                    if let Some(pos) = self.find_break_at(pattern, max_pos) {
                        return pos;
                    }
                }
            }
            BreakPreference::Whitespace => {}
        }

        // Fall back to whitespace
        if let Some(pos) = self.find_last_whitespace(max_pos) {
            return pos;
        }

        // Hard break at max_chars
        if self.buffer.len() > self.config.max_chars {
            return self.config.max_chars;
        }

        0
    }

    fn find_break_at(&self, pattern: &str, max_pos: usize) -> Option<usize> {
        let search_range = &self.buffer[..max_pos];
        // Find last occurrence of pattern
        search_range.rfind(pattern).map(|pos| pos + pattern.len())
    }

    fn find_last_whitespace(&self, max_pos: usize) -> Option<usize> {
        let search_range = &self.buffer[..max_pos];
        search_range.rfind(char::is_whitespace).map(|pos| pos + 1)
    }

    fn track_code_fences(&mut self, text: &str) {
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                if self.in_code_fence {
                    self.in_code_fence = false;
                    self.code_fence_marker = None;
                } else {
                    self.in_code_fence = true;
                    // Store the exact fence marker (e.g., ```rust)
                    self.code_fence_marker = Some(trimmed.to_string());
                }
            }
        }
    }
}

/// Coalescer for merging multiple chunks before sending.
pub struct ChunkCoalescer {
    config: CoalesceConfig,
    buffer: Vec<String>,
    total_chars: usize,
    last_push: Instant,
}

impl ChunkCoalescer {
    pub fn new(config: CoalesceConfig) -> Self {
        Self {
            config,
            buffer: Vec::new(),
            total_chars: 0,
            last_push: Instant::now(),
        }
    }

    /// Push a chunk, returning merged text if ready to send.
    pub fn push(&mut self, chunk: StreamChunk) -> Option<String> {
        self.buffer.push(chunk.text.clone());
        self.total_chars += chunk.text.len();
        self.last_push = Instant::now();

        if chunk.is_final {
            return Some(self.drain());
        }

        if self.total_chars >= self.config.max_chars {
            return Some(self.drain());
        }

        None
    }

    /// Check if we should flush due to idle timeout.
    pub fn should_flush(&self) -> bool {
        !self.buffer.is_empty()
            && self.total_chars >= self.config.min_chars
            && self.last_push.elapsed() > Duration::from_millis(self.config.idle_ms)
    }

    /// Drain and return accumulated text.
    pub fn drain(&mut self) -> String {
        let text = self.buffer.join("");
        self.buffer.clear();
        self.total_chars = 0;
        text
    }

    /// Check if buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunker_basic() {
        let config = BlockStreamingConfig {
            min_chars: 10,
            max_chars: 50,
            break_preference: BreakPreference::Paragraph,
            idle_ms: 100,
            enabled: true,
        };
        let mut chunker = BlockChunker::new(config);

        // Short text - no chunks yet
        let chunks = chunker.push("Hello");
        assert!(chunks.is_empty(), "Expected no chunks for short text");

        // Build up to min_chars but no break point
        let chunks = chunker.push(" world, this is more text");
        // May or may not emit depending on break points
        // Just verify we can push without panic

        // Paragraph break with enough content - should emit on flush
        chunker.push("\n\nNext paragraph with enough text to reach minimum.");
        let final_chunk = chunker.flush();
        assert!(final_chunk.is_some());
        assert!(final_chunk.unwrap().is_final);
    }

    #[test]
    fn test_chunker_respects_max_chars() {
        let config = BlockStreamingConfig {
            min_chars: 5,
            max_chars: 20,
            break_preference: BreakPreference::Whitespace,
            idle_ms: 100,
            enabled: true,
        };
        let mut chunker = BlockChunker::new(config);

        let chunks = chunker.push("This is a long text that should be split into multiple chunks.");

        // Should have emitted chunks
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(chunk.text.len() <= 25); // max_chars + some buffer
        }
    }

    #[test]
    fn test_chunker_code_fence_tracking() {
        let config = BlockStreamingConfig {
            min_chars: 5,
            max_chars: 100,
            break_preference: BreakPreference::Newline,
            idle_ms: 100,
            enabled: true,
        };
        let mut chunker = BlockChunker::new(config);

        chunker.push("```rust\nlet x = 1;\n```");
        // After complete fence, should not be in code fence
        assert!(!chunker.in_code_fence);

        chunker.push("```python\nprint('hello')");
        // Unclosed fence
        assert!(chunker.in_code_fence);
    }

    #[test]
    fn test_chunker_flush() {
        let config = BlockStreamingConfig::default();
        let mut chunker = BlockChunker::new(config);

        chunker.push("Final text");
        let final_chunk = chunker.flush();

        assert!(final_chunk.is_some());
        let chunk = final_chunk.unwrap();
        assert!(chunk.is_final);
        assert_eq!(chunk.text, "Final text");
    }

    #[test]
    fn test_coalescer_basic() {
        let config = CoalesceConfig {
            min_chars: 5,
            max_chars: 100,
            idle_ms: 100,
        };
        let mut coalescer = ChunkCoalescer::new(config);

        // Non-final chunks accumulate
        assert!(coalescer
            .push(StreamChunk {
                text: "Hello ".into(),
                is_final: false,
            })
            .is_none());

        // Final chunk triggers drain
        let result = coalescer.push(StreamChunk {
            text: "world!".into(),
            is_final: true,
        });

        assert_eq!(result, Some("Hello world!".into()));
    }

    #[test]
    fn test_coalescer_max_chars() {
        let config = CoalesceConfig {
            min_chars: 5,
            max_chars: 15,
            idle_ms: 100,
        };
        let mut coalescer = ChunkCoalescer::new(config);

        coalescer.push(StreamChunk {
            text: "Hello ".into(),
            is_final: false,
        });

        // This should trigger drain due to max_chars
        let result = coalescer.push(StreamChunk {
            text: "wonderful world!".into(),
            is_final: false,
        });

        assert!(result.is_some());
    }
}
