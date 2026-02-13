use sha2::{Digest, Sha256};

/// A chunk of text extracted from a Markdown file
#[derive(Debug, Clone, PartialEq)]
pub struct TextChunk {
    /// The chunk text content
    pub text: String,
    /// 1-based start line in the original file
    pub start_line: usize,
    /// 1-based end line in the original file (inclusive)
    pub end_line: usize,
    /// SHA-256 hash of the chunk text (hex string)
    pub hash: String,
}

/// Configuration for the chunker
pub struct ChunkerConfig {
    /// Target chunk size in characters (proxy for ~tokens, using 4 chars ≈ 1 token)
    /// Default: 1600 (≈ 400 tokens)
    pub target_size: usize,
    /// Overlap size in characters
    /// Default: 320 (≈ 80 tokens)
    pub overlap_size: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            target_size: 1600,
            overlap_size: 320,
        }
    }
}

/// Split markdown content into overlapping chunks.
///
/// Strategy:
/// 1. Split by Markdown headings (# ## ### etc.) as natural boundaries
/// 2. If a section fits within target_size, it becomes one chunk
/// 3. If a section exceeds target_size, split it by paragraphs (double newline)
/// 4. If a paragraph still exceeds target_size, split by fixed window with overlap
/// 5. Adjacent chunks overlap by overlap_size characters from the end of the previous chunk
///
/// Returns empty vec for empty input.
pub fn chunk_markdown(content: &str, config: &ChunkerConfig) -> Vec<TextChunk> {
    if content.is_empty() {
        return Vec::new();
    }

    let target_size = config.target_size.max(1);
    let overlap_size = config.overlap_size.min(target_size.saturating_sub(1));
    let line_starts = collect_line_starts(content);
    let sections = split_sections(content, &line_starts);
    let mut chunks = Vec::new();

    for section in sections {
        let section_text = &content[section.start_offset..section.end_offset];
        let chunk_ranges = if section_text.len() <= target_size {
            std::iter::once(0..section_text.len()).collect()
        } else {
            split_large_section(section_text, target_size, overlap_size)
        };

        for chunk_range in chunk_ranges {
            if chunk_range.start >= chunk_range.end {
                continue;
            }
            let abs_start = section.start_offset + chunk_range.start;
            let abs_end = section.start_offset + chunk_range.end;
            let text = content[abs_start..abs_end].to_owned();
            let start_line = line_number_for_offset(&line_starts, abs_start);
            let end_line = line_number_for_offset(&line_starts, abs_end.saturating_sub(1));
            chunks.push(TextChunk {
                hash: compute_hash(&text),
                text,
                start_line,
                end_line,
            });
        }
    }

    chunks
}

#[derive(Debug, Clone)]
struct SectionRange {
    start_offset: usize,
    end_offset: usize,
}

fn collect_line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, ch) in content.char_indices() {
        if ch == '\n' && idx + 1 < content.len() {
            starts.push(idx + 1);
        }
    }
    starts
}

fn line_number_for_offset(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(idx) => idx + 1,
        Err(idx) => idx,
    }
}

fn split_sections(content: &str, line_starts: &[usize]) -> Vec<SectionRange> {
    let mut sections = Vec::new();
    let mut section_start = 0;
    let mut offset = 0;

    for line in content.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        if is_heading_line(line) && line_start != section_start {
            sections.push(SectionRange {
                start_offset: section_start,
                end_offset: line_start,
            });
            section_start = line_start;
        }
    }

    if section_start < content.len() {
        sections.push(SectionRange {
            start_offset: section_start,
            end_offset: content.len(),
        });
    }

    sections
        .into_iter()
        .filter(|section| {
            section.start_offset < section.end_offset
                && line_number_for_offset(line_starts, section.end_offset.saturating_sub(1))
                    >= line_number_for_offset(line_starts, section.start_offset)
        })
        .collect()
}

fn is_heading_line(line: &str) -> bool {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let hash_count = trimmed.chars().take_while(|ch| *ch == '#').count();
    hash_count > 0 && trimmed.chars().nth(hash_count) == Some(' ')
}

fn split_large_section(
    text: &str,
    target_size: usize,
    overlap_size: usize,
) -> Vec<std::ops::Range<usize>> {
    let paragraph_ranges = split_paragraph_ranges(text);
    let mut core_ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut current: Option<std::ops::Range<usize>> = None;

    for paragraph in paragraph_ranges {
        let paragraph_len = paragraph.end - paragraph.start;
        if paragraph_len > target_size {
            if let Some(cur) = current.take() {
                core_ranges.push(cur);
            }
            core_ranges.extend(split_fixed_window(
                text,
                paragraph.start,
                paragraph.end,
                target_size,
                overlap_size,
            ));
            continue;
        }

        if let Some(cur) = current.as_mut() {
            if paragraph.end - cur.start <= target_size {
                cur.end = paragraph.end;
            } else {
                core_ranges.push(cur.clone());
                *cur = paragraph;
            }
        } else {
            current = Some(paragraph);
        }
    }

    if let Some(cur) = current {
        core_ranges.push(cur);
    }

    let mut with_overlap: Vec<std::ops::Range<usize>> = Vec::new();
    for range in core_ranges {
        let mut start = range.start;
        if let Some(prev) = with_overlap.last() {
            let desired = prev.end.saturating_sub(overlap_size);
            if start > desired {
                start = desired;
            }
        }
        with_overlap.push(start..range.end);
    }

    with_overlap
}

fn split_paragraph_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut start = 0;
    let mut cursor = 0;

    while let Some(pos) = text[cursor..].find("\n\n") {
        let split_end = cursor + pos + 2;
        ranges.push(start..split_end);
        start = split_end;
        cursor = split_end;
    }

    if start < text.len() {
        ranges.push(start..text.len());
    }

    ranges
}

fn split_fixed_window(
    text: &str,
    start: usize,
    end: usize,
    target_size: usize,
    overlap_size: usize,
) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = start;
    let step = target_size.saturating_sub(overlap_size).max(1);

    while cursor < end {
        let window_end = (cursor + target_size).min(end);
        let mut split_end = window_end;

        if window_end < end && window_end > cursor {
            let window = &text[cursor..window_end];
            if let Some(last_space) = window.rfind(' ') {
                if last_space > 0 {
                    split_end = cursor + last_space;
                }
            }
        }

        if split_end <= cursor {
            split_end = window_end;
        }
        if split_end <= cursor {
            break;
        }

        ranges.push(cursor..split_end);
        if split_end >= end {
            break;
        }

        cursor = cursor.saturating_add(step);
    }

    ranges
}

fn compute_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(target_size: usize, overlap_size: usize) -> ChunkerConfig {
        ChunkerConfig {
            target_size,
            overlap_size,
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        let chunks = chunk_markdown("", &cfg(100, 20));
        assert!(chunks.is_empty());
    }

    #[test]
    fn small_content_single_chunk() {
        let content = "# Title\n\nHello markdown.";
        let chunks = chunk_markdown(content, &cfg(100, 20));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, content);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
    }

    #[test]
    fn heading_splits() {
        let content = "## A\n\nalpha\n\n## B\n\nbeta";
        let chunks = chunk_markdown(content, &cfg(20, 5));
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.starts_with("## A"));
        assert!(chunks[1].text.starts_with("## B"));
    }

    #[test]
    fn large_section_splits_by_paragraphs() {
        let content = "# Title\n\npara one\n\npara two\n\npara three";
        let chunks = chunk_markdown(content, &cfg(18, 4));
        assert!(chunks.len() >= 2);
        assert!(chunks
            .iter()
            .all(|c| c.text.starts_with("# Title") || c.text.contains("para")));
    }

    #[test]
    fn huge_paragraph_fixed_window() {
        let content = "# T\n\nabcdefghijklmnopqrstuvwxyz";
        let chunks = chunk_markdown(content, &cfg(10, 3));
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|c| !c.text.is_empty()));
    }

    #[test]
    fn overlap_between_paragraph_chunks() {
        let content = "# T\n\nfirst paragraph has enough text\n\nsecond paragraph has enough text";
        let chunks = chunk_markdown(content, &cfg(32, 8));
        assert!(chunks.len() >= 2);
        let a = &chunks[0].text;
        let b = &chunks[1].text;
        let tail = &a[a.len().saturating_sub(8)..];
        assert!(b.starts_with(tail));
    }

    #[test]
    fn no_overlap_across_headings() {
        let content = "# A\n\n1111111111\n\n# B\n\n2222222222";
        let chunks = chunk_markdown(content, &cfg(64, 4));
        assert_eq!(chunks.len(), 2);
        let tail = &chunks[0].text[chunks[0].text.len().saturating_sub(4)..];
        assert!(!chunks[1].text.starts_with(tail));
    }

    #[test]
    fn line_numbers_correct() {
        let content = "intro\n\n# A\nline a\n\n# B\nline b";
        let chunks = chunk_markdown(content, &cfg(100, 10));
        assert_eq!(chunks.len(), 3);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 2));
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (3, 5));
        assert_eq!((chunks[2].start_line, chunks[2].end_line), (6, 7));
    }

    #[test]
    fn hash_is_deterministic() {
        let content = "# A\n\nhello";
        let first = chunk_markdown(content, &cfg(100, 10));
        let second = chunk_markdown(content, &cfg(100, 10));
        assert_eq!(first[0].hash, second[0].hash);
    }

    #[test]
    fn hash_differs_for_different_content() {
        let a = chunk_markdown("# A\n\nhello", &cfg(100, 10));
        let b = chunk_markdown("# A\n\nworld", &cfg(100, 10));
        assert_ne!(a[0].hash, b[0].hash);
    }

    #[test]
    fn preamble_before_first_heading() {
        let content = "intro line\n\n# A\nbody";
        let chunks = chunk_markdown(content, &cfg(100, 10));
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.starts_with("intro"));
        assert!(chunks[1].text.starts_with("# A"));
    }

    #[test]
    fn default_config_values() {
        let config = ChunkerConfig::default();
        assert_eq!(config.target_size, 1600);
        assert_eq!(config.overlap_size, 320);
    }

    #[test]
    fn hash_helper_smoke() {
        let h = compute_hash("abc");
        assert_eq!(h.len(), 64);
    }
}
