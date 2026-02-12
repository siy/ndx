use std::collections::HashSet;

/// Extract unique trigrams from content bytes.
/// Skips trigrams containing null bytes (binary indicator).
pub fn extract_trigrams(content: &[u8]) -> HashSet<[u8; 3]> {
    let mut set = HashSet::new();
    for window in content.windows(3) {
        if !window.contains(&0) {
            set.insert([window[0], window[1], window[2]]);
        }
    }
    set
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

/// Encode a sorted list of doc IDs as packed little-endian u32 bytes.
pub fn encode_posting_list(ids: &[u32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ids.len() * 4);
    for &id in ids {
        buf.extend_from_slice(&id.to_le_bytes());
    }
    buf
}

/// Decode packed little-endian u32 bytes into doc IDs.
pub fn decode_posting_list(data: &[u8]) -> Vec<u32> {
    data.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Intersect multiple sorted posting lists. Smallest-first for efficiency.
pub fn intersect_posting_lists(lists: &[Vec<u32>]) -> Vec<u32> {
    if lists.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&Vec<u32>> = lists.iter().collect();
    sorted.sort_by_key(|l| l.len());

    let mut result = sorted[0].clone();
    for list in &sorted[1..] {
        result = intersect_two(&result, list);
        if result.is_empty() {
            break;
        }
    }
    result
}

/// Intersect two sorted lists via merge.
fn intersect_two(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
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
