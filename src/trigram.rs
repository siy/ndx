use std::collections::{HashMap, HashSet};

/// A posting entry stores a (doc_id, line_num) pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PostingEntry {
    pub doc_id: u32,
    pub line_num: u32,
}

/// Extract trigrams with line-level positions from content bytes.
/// Returns a map of trigram → sorted, deduplicated line numbers (1-based).
/// Skips trigrams containing null bytes (binary indicator).
pub fn extract_trigrams_with_lines(content: &[u8]) -> HashMap<[u8; 3], Vec<u32>> {
    let mut map: HashMap<[u8; 3], HashSet<u32>> = HashMap::new();
    for (line_idx, line) in content.split(|&b| b == b'\n').enumerate() {
        let line_num = (line_idx + 1) as u32;
        for window in line.windows(3) {
            if !window.contains(&0) {
                map.entry([window[0], window[1], window[2]])
                    .or_default()
                    .insert(line_num);
            }
        }
    }
    map.into_iter()
        .map(|(tri, lines)| {
            let mut sorted: Vec<u32> = lines.into_iter().collect();
            sorted.sort_unstable();
            (tri, sorted)
        })
        .collect()
}

/// Extract trigrams from a search query string.
/// Returns None if the query is too short (< 3 bytes) to produce trigrams.
pub fn query_trigrams(query: &str) -> Option<Vec<[u8; 3]>> {
    let bytes = query.as_bytes();
    if bytes.len() < 3 {
        return None;
    }
    let trigrams: HashSet<[u8; 3]> = bytes
        .windows(3)
        .map(|w| [w[0], w[1], w[2]])
        .collect();
    Some(trigrams.into_iter().collect())
}

/// Encode a sorted list of posting entries as packed little-endian bytes.
/// Each entry is 8 bytes: 4 for doc_id + 4 for line_num.
pub fn encode_posting_list(entries: &[PostingEntry]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(entries.len() * 8);
    for e in entries {
        buf.extend_from_slice(&e.doc_id.to_le_bytes());
        buf.extend_from_slice(&e.line_num.to_le_bytes());
    }
    buf
}

/// Decode packed little-endian bytes into posting entries.
pub fn decode_posting_list(data: &[u8]) -> Vec<PostingEntry> {
    data.chunks_exact(8)
        .map(|c| PostingEntry {
            doc_id: u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
            line_num: u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
        })
        .collect()
}

/// Intersect multiple posting lists on doc_id, union line numbers per doc.
/// Input lists must be sorted by (doc_id, line_num).
pub fn intersect_posting_lists(lists: &[Vec<PostingEntry>]) -> Vec<PostingEntry> {
    if lists.is_empty() {
        return Vec::new();
    }
    if lists.len() == 1 {
        return lists[0].clone();
    }

    // Build per-list maps of doc_id → line_nums
    let maps: Vec<HashMap<u32, Vec<u32>>> = lists
        .iter()
        .map(|list| {
            let mut m: HashMap<u32, Vec<u32>> = HashMap::new();
            for e in list {
                m.entry(e.doc_id).or_default().push(e.line_num);
            }
            m
        })
        .collect();

    // Start with doc_ids from the smallest map
    let mut sorted_indices: Vec<usize> = (0..maps.len()).collect();
    sorted_indices.sort_by_key(|&i| maps[i].len());

    let mut common_doc_ids: HashSet<u32> = maps[sorted_indices[0]].keys().copied().collect();
    for &idx in &sorted_indices[1..] {
        common_doc_ids.retain(|id| maps[idx].contains_key(id));
        if common_doc_ids.is_empty() {
            return Vec::new();
        }
    }

    // For common doc_ids, union all line numbers
    let mut doc_ids: Vec<u32> = common_doc_ids.into_iter().collect();
    doc_ids.sort_unstable();

    let mut result = Vec::new();
    for doc_id in doc_ids {
        let mut lines: HashSet<u32> = HashSet::new();
        for map in &maps {
            if let Some(line_nums) = map.get(&doc_id) {
                lines.extend(line_nums);
            }
        }
        let mut sorted_lines: Vec<u32> = lines.into_iter().collect();
        sorted_lines.sort_unstable();
        for line_num in sorted_lines {
            result.push(PostingEntry { doc_id, line_num });
        }
    }

    result
}

/// Returns true if content appears to be binary (null byte in first 8KB).
pub fn is_binary(content: &[u8]) -> bool {
    let check_len = content.len().min(8192);
    content[..check_len].contains(&0)
}

/// Extract the longest literal substring from a regex pattern.
/// Splits on metacharacters and returns the longest piece >= 3 chars.
pub fn extract_longest_literal(pattern: &str) -> Option<&str> {
    const META: &[char] = &[
        '.', '*', '+', '?', '[', ']', '(', ')', '{', '}', '|', '^', '$', '\\',
    ];
    pattern
        .split(|c: char| META.contains(&c))
        .filter(|s| s.len() >= 3)
        .max_by_key(|s| s.len())
}

/// Returns true if the pattern has no regex metacharacters (is a plain literal).
pub fn is_literal_pattern(pattern: &str) -> bool {
    const META: &[char] = &[
        '.', '*', '+', '?', '[', ']', '(', ')', '{', '}', '|', '^', '$', '\\',
    ];
    !pattern.chars().any(|c| META.contains(&c))
}
