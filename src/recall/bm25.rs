//! BM25 lexical ranking for the recall palace.
//!
//! Replaces the drawer-text trigram index with an Okapi BM25 scorer over
//! a simple unicode-aware tokenizer. Trigrams remain the right tool for
//! code substring search (daemon `trigram.rs`); drawer text is natural
//! language where BM25 is the industry baseline and pairs well with the
//! semantic channel under RRF (Anthropic's contextual retrieval report
//! validates this combination for small-to-medium corpora).

use std::collections::HashMap;

/// Okapi BM25 tuning parameters. Standard defaults; also cited in the
/// Anthropic contextual retrieval report.
pub const BM25_K1: f32 = 1.2;
pub const BM25_B: f32 = 0.75;

/// Minimum token length. Single-character tokens are discarded: they
/// collide across every drawer and inflate postings lists without
/// carrying discriminative signal.
pub const MIN_TOKEN_LEN: usize = 2;

/// Short English stopword list. Conservative — just the most common
/// function words, no verbs or pronouns that could matter in a search
/// query. Hand-curated rather than pulled from a crate to keep the
/// dependency surface thin and the behaviour deterministic.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from",
    "has", "have", "he", "in", "is", "it", "its", "of", "on", "or", "she",
    "that", "the", "they", "this", "to", "was", "were", "will", "with",
];

fn is_stopword(tok: &str) -> bool {
    STOPWORDS.binary_search(&tok).is_ok()
}

/// Tokenize a string for BM25 indexing/querying:
///   - split on any non-alphanumeric character (unicode-aware via
///     `char::is_alphanumeric`),
///   - lowercase,
///   - drop tokens shorter than `MIN_TOKEN_LEN`,
///   - drop stopwords.
///
/// Returns tokens in document order (duplicates preserved so callers
/// can compute term frequency).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_lowercase();
        if lower.chars().count() < MIN_TOKEN_LEN {
            continue;
        }
        if is_stopword(&lower) {
            continue;
        }
        out.push(lower);
    }
    out
}

/// Build (term → term-frequency-in-document) from a tokenized document.
pub fn term_frequencies(tokens: &[String]) -> HashMap<String, u32> {
    let mut tf: HashMap<String, u32> = HashMap::new();
    for t in tokens {
        *tf.entry(t.clone()).or_insert(0) += 1;
    }
    tf
}

/// BM25 IDF component, `ln(1 + (N - df + 0.5) / (df + 0.5))`.
/// Always non-negative for df ∈ [0, N].
pub fn idf(n: u64, df: u64) -> f32 {
    let n = n as f32;
    let df = df as f32;
    ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
}

/// BM25 term score for a single (document, token) combination.
///
/// * `tf`    — term frequency inside the document
/// * `dl`    — length (token count) of the document
/// * `avgdl` — corpus-wide average document length
/// * `idf`   — precomputed IDF for the token
pub fn term_score(tf: u32, dl: u32, avgdl: f32, idf: f32) -> f32 {
    if tf == 0 {
        return 0.0;
    }
    let tf = tf as f32;
    let dl = dl as f32;
    // Guard against zero avgdl (empty corpus); normalization collapses to k1*(1-b)+b when dl=0.
    let denom_norm = if avgdl > 0.0 { dl / avgdl } else { 1.0 };
    let denom = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * denom_norm);
    idf * ((BM25_K1 + 1.0) * tf / denom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_on_punctuation() {
        // Underscore is non-alphanumeric per `char::is_alphanumeric`, so
        // `bar_baz` splits into two tokens. This matches the design goal
        // of treating identifier boundaries as token boundaries (a user
        // searching for "baz" should find `bar_baz`).
        let toks = tokenize("hello, world! foo-bar_baz");
        assert_eq!(toks, vec!["hello", "world", "foo", "bar", "baz"]);
    }

    #[test]
    fn tokenize_lowercases() {
        let toks = tokenize("Rust IS Great");
        assert_eq!(toks, vec!["rust", "great"]);
    }

    #[test]
    fn tokenize_drops_short_tokens() {
        let toks = tokenize("i a go no");
        assert_eq!(toks, vec!["go", "no"]);
    }

    #[test]
    fn tokenize_drops_stopwords() {
        let toks = tokenize("the quick brown fox was here");
        assert_eq!(toks, vec!["quick", "brown", "fox", "here"]);
    }

    #[test]
    fn tokenize_handles_unicode() {
        // Non-ASCII letters are alphanumeric per `char::is_alphanumeric`
        // and should survive tokenization.
        let toks = tokenize("café naïve — Москва");
        assert_eq!(toks, vec!["café", "naïve", "москва"]);
    }

    #[test]
    fn tokenize_handles_numbers_and_mixed() {
        let toks = tokenize("R-142 covers BM25 & v2");
        assert_eq!(toks, vec!["142", "covers", "bm25", "v2"]);
    }

    #[test]
    fn term_frequencies_counts_duplicates() {
        let tokens: Vec<String> = ["foo", "bar", "foo", "baz", "foo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let tf = term_frequencies(&tokens);
        assert_eq!(tf.get("foo"), Some(&3));
        assert_eq!(tf.get("bar"), Some(&1));
        assert_eq!(tf.get("baz"), Some(&1));
    }

    #[test]
    fn bm25_score_hand_computed() {
        // Three-document corpus, single query term "foo".
        // Doc lengths (token counts): 4, 6, 3. avgdl = 13 / 3 ≈ 4.3333.
        // Query term appears in docs 1 and 2 with tf = 2 and 1 respectively.
        // df = 2, N = 3.
        let n = 3u64;
        let df = 2u64;
        let avgdl = 13.0 / 3.0;

        let idf_val = idf(n, df);
        // idf = ln( (3 - 2 + 0.5) / (2 + 0.5) + 1 ) = ln(1.6) ≈ 0.47000363
        assert!((idf_val - 0.470_003_63).abs() < 1e-5, "idf={}", idf_val);

        // Score for doc1: tf=2, dl=4
        let s1 = term_score(2, 4, avgdl, idf_val);
        // denom_norm = 4/4.3333 ≈ 0.92308
        // denom = 2 + 1.2 * (1 - 0.75 + 0.75 * 0.92308)
        //       = 2 + 1.2 * (0.25 + 0.69231)
        //       = 2 + 1.2 * 0.94231
        //       = 2 + 1.13077 = 3.13077
        // score = 0.47000 * (1.2 + 1) * 2 / 3.13077
        //       = 0.47000 * 2.2 * 2 / 3.13077
        //       = 0.47000 * 4.4 / 3.13077
        //       = 2.068 / 3.13077 = 0.66053
        assert!((s1 - 0.660_53).abs() < 1e-3, "s1={}", s1);

        // Score for doc2: tf=1, dl=6
        let s2 = term_score(1, 6, avgdl, idf_val);
        // denom_norm = 6/4.3333 = 1.38462
        // denom = 1 + 1.2 * (0.25 + 0.75 * 1.38462)
        //       = 1 + 1.2 * (0.25 + 1.03846)
        //       = 1 + 1.2 * 1.28846
        //       = 1 + 1.54615 = 2.54615
        // score = 0.47000 * 2.2 * 1 / 2.54615
        //       = 1.034 / 2.54615 = 0.40611
        assert!((s2 - 0.406_11).abs() < 1e-3, "s2={}", s2);

        // Doc with tf=0 scores 0.
        assert_eq!(term_score(0, 3, avgdl, idf_val), 0.0);
        // Doc1 outranks doc2 because its tf is higher and it's shorter.
        assert!(s1 > s2);
    }

    #[test]
    fn bm25_empty_corpus_is_safe() {
        // avgdl=0 should not panic and returns a finite score.
        let s = term_score(1, 0, 0.0, 1.0);
        assert!(s.is_finite());
    }
}
