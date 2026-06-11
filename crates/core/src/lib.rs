#![cfg_attr(docsrs, feature(doc_auto_cfg))]
//! Shared types, gram extraction, storage traits, and scan verification.

use anyhow::{Context, Result as AnyhowResult};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;

pub type DocId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Raw,
    Gzip,
    Zstd,
}

/// Detect compression by magic bytes; key extensions are not trusted.
/// Gzip requires the deflate method byte (1f 8b 08) — the only method the
/// format defines. Zstd covers both regular frames (28 b5 2f fd) and
/// skippable frames (5? 2a 4d 18), which may legally come first.
pub fn detect_codec(bytes: &[u8]) -> Codec {
    if bytes.starts_with(&[0x1f, 0x8b, 0x08]) {
        Codec::Gzip
    } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
        || (bytes.len() >= 4
            && bytes[0] & 0xf0 == 0x50
            && bytes[1] == 0x2a
            && bytes[2] == 0x4d
            && bytes[3] == 0x18)
    {
        Codec::Zstd
    } else {
        Codec::Raw
    }
}

/// Transparently decompress an object body. Gzip uses `MultiGzDecoder`
/// because AWS log deliveries (ALB, `CloudTrail`, `CloudFront`) concatenate
/// gzip members; a single-member decoder silently truncates them. A decode
/// error after some members already decoded (trailing padding or a truncated
/// tail) salvages the decoded text with a warning — for grep, partial
/// coverage beats dropping the object.
pub fn decode_body(key: &str, bytes: Vec<u8>) -> AnyhowResult<Vec<u8>> {
    match detect_codec(&bytes) {
        Codec::Raw => Ok(bytes),
        Codec::Gzip => {
            let mut out = Vec::new();
            match std::io::Read::read_to_end(
                &mut flate2::read::MultiGzDecoder::new(bytes.as_slice()),
                &mut out,
            ) {
                Ok(_) => Ok(out),
                Err(err) if !out.is_empty() => {
                    eprintln!(
                        "warning: {key}: gzip stream ends in garbage ({err}); \
                         searching the {} bytes that decoded",
                        out.len()
                    );
                    Ok(out)
                }
                Err(err) => {
                    Err(anyhow::Error::new(err).context(format!("gzip decode failed for {key}")))
                }
            }
        }
        Codec::Zstd => zstd::stream::decode_all(bytes.as_slice())
            .with_context(|| format!("zstd decode failed for {key}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strategy {
    Trigram,
    Sparse,
}

/// Every overlapping 3-byte window as raw bytes (sorted, deduped). <3 bytes => empty.
/// Windows pack big-endian into u32 so sort+dedup run over integers (u32
/// order == lexicographic byte order) and only distinct grams allocate.
pub fn trigram_grams_bytes(data: &[u8]) -> Vec<Vec<u8>> {
    let mut packed: Vec<u32> = data
        .windows(3)
        .map(|w| u32::from(w[0]) << 16 | u32::from(w[1]) << 8 | u32::from(w[2]))
        .collect();
    packed.sort_unstable();
    packed.dedup();
    packed
        .into_iter()
        .map(|g| vec![(g >> 16) as u8, (g >> 8) as u8, g as u8])
        .collect()
}

/// Stable u64 hash of an n-gram's bytes. Deterministic across runs/platforms
/// (used as the on-disk + in-memory gram key).
pub fn hash_ngram(gram: &[u8]) -> u64 {
    rapidhash::v3::rapidhash_v3(gram)
}

/// Deterministic weight of an adjacent byte pair. Drives sparse-ngram
/// boundary selection. Only affects selectivity, never correctness.
pub(crate) fn pair_weight(a: u8, b: u8) -> u32 {
    rapidhash::v3::rapidhash_v3(&[a, b]) as u32
}

/// `build_all` as raw gram byte strings (sorted, deduped). Index-time.
pub fn sparse_grams_all_bytes(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if data.len() < 2 {
        return out;
    }
    let weights: Vec<u32> = data.windows(2).map(|w| pair_weight(w[0], w[1])).collect();
    let n = weights.len();
    for i in 0..n {
        out.push(data[i..i + 2].to_vec());
        let mut interior_max: u32 = 0;
        for j in (i + 1)..n {
            if j > i + 1 {
                interior_max = interior_max.max(weights[j - 1]);
            }
            if interior_max >= weights[i] {
                break;
            }
            if weights[j] > interior_max {
                out.push(data[i..j + 2].to_vec());
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// `build_covering` as raw gram byte strings (sorted, deduped). Query-time.
pub fn sparse_grams_covering_bytes(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if data.len() < 2 {
        return out;
    }
    let weights: Vec<u32> = data.windows(2).map(|w| pair_weight(w[0], w[1])).collect();
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..weights.len() {
        while let Some(&top) = stack.last() {
            if weights[top] <= weights[i] {
                out.push(data[top..i + 2].to_vec());
                if weights[top] == weights[i] {
                    stack.pop();
                    break;
                }
                stack.pop();
            } else {
                break;
            }
        }
        stack.push(i);
    }
    while stack.len() > 1 {
        let top = stack.pop().unwrap();
        if let Some(&prev) = stack.last() {
            out.push(data[prev..top + 2].to_vec());
        }
    }
    if let Some(&pos) = stack.last() {
        out.push(data[pos..pos + 2].to_vec());
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Index-time grams for a strategy.
pub fn grams_index(data: &[u8], s: Strategy) -> Vec<Vec<u8>> {
    match s {
        Strategy::Trigram => trigram_grams_bytes(data),
        Strategy::Sparse => sparse_grams_all_bytes(data),
    }
}

/// Query-time grams for a strategy (trigram has no separate covering form).
pub fn grams_query(data: &[u8], s: Strategy) -> Vec<Vec<u8>> {
    match s {
        Strategy::Trigram => trigram_grams_bytes(data),
        Strategy::Sparse => sparse_grams_covering_bytes(data),
    }
}

#[cfg(test)]
mod invariant_grams {
    use super::{hash_ngram, sparse_grams_all_bytes, sparse_grams_covering_bytes};

    pub(super) fn extract_sparse_ngrams_all(data: &[u8]) -> Vec<(u64, usize)> {
        sparse_grams_all_bytes(data)
            .iter()
            .map(|g| (hash_ngram(g), g.len()))
            .collect()
    }

    pub(super) fn extract_sparse_ngrams_covering(data: &[u8]) -> Vec<(u64, usize)> {
        sparse_grams_covering_bytes(data)
            .iter()
            .map(|g| (hash_ngram(g), g.len()))
            .collect()
    }
}

/// A source of documents for INDEX BUILDS, which need full enumeration.
/// Implemented by a local dir (tests) and S3 (prod).
pub trait Corpus {
    /// All document ids with their keys (object key / file path).
    fn docs(&self) -> &[(DocId, String)];
    /// Fetch the full bytes of one document.
    fn fetch(&self, id: DocId) -> AnyhowResult<Vec<u8>>;
    /// Fetch many docs concurrently. Result order is NOT guaranteed; each item
    /// carries its `DocId`. Implementations may return fewer docs than
    /// requested when a doc vanished between indexing and fetching.
    /// Default = sequential, fail-fast.
    fn fetch_many(&self, ids: &[DocId]) -> anyhow::Result<Vec<(DocId, Vec<u8>)>> {
        ids.iter().map(|&id| Ok((id, self.fetch(id)?))).collect()
    }
}

/// Fetches documents by key for SEARCH verification — no enumeration, no
/// doc table. `consume` receives the index into `keys` plus the body, as
/// fetches complete (order NOT guaranteed). Implementations may fetch
/// concurrently and may skip vanished docs; the first `consume` error
/// aborts the remaining fetches.
pub trait DocFetcher {
    fn fetch_each(
        &self,
        keys: &[String],
        consume: &mut dyn FnMut(usize, Vec<u8>) -> AnyhowResult<()>,
    ) -> AnyhowResult<()>;
}

pub trait BlobStore {
    fn put(&self, name: &str, bytes: &[u8]) -> AnyhowResult<()>;
    /// `Ok(None)` = blob does not exist. Transient store failures are `Err`
    /// so callers never mistake an outage for an empty store.
    fn get(&self, name: &str) -> AnyhowResult<Option<Vec<u8>>>;
    fn get_range(&self, name: &str, start: u64, len: u64) -> AnyhowResult<Vec<u8>>;
    /// Fetch many byte ranges of one blob, preserving order. Implementations
    /// may fetch concurrently. Default = sequential.
    fn get_ranges(&self, name: &str, ranges: &[(u64, u64)]) -> AnyhowResult<Vec<Vec<u8>>> {
        ranges
            .iter()
            .map(|&(start, len)| self.get_range(name, start, len))
            .collect()
    }
}

#[cfg(any(test, feature = "testutil"))]
pub mod testutil {
    use super::{Corpus, DocId};
    use anyhow::Result;

    pub struct MemCorpus {
        docs: Vec<(DocId, String)>,
        bodies: Vec<Vec<u8>>,
    }

    impl MemCorpus {
        pub fn new(docs: Vec<(DocId, String)>, bodies: Vec<Vec<u8>>) -> MemCorpus {
            assert_eq!(docs.len(), bodies.len());
            MemCorpus { docs, bodies }
        }
    }

    impl Corpus for MemCorpus {
        fn docs(&self) -> &[(DocId, String)] {
            &self.docs
        }

        fn fetch(&self, id: DocId) -> Result<Vec<u8>> {
            Ok(self.bodies[id as usize].clone())
        }
    }

    impl crate::DocFetcher for MemCorpus {
        fn fetch_each(
            &self,
            keys: &[String],
            consume: &mut dyn FnMut(usize, Vec<u8>) -> Result<()>,
        ) -> Result<()> {
            for (idx, key) in keys.iter().enumerate() {
                let (id, _) = self
                    .docs
                    .iter()
                    .find(|(_, k)| k == key)
                    .ok_or_else(|| anyhow::anyhow!("unknown key {key}"))?;
                consume(idx, self.bodies[*id as usize].clone())?;
            }
            Ok(())
        }
    }
}

pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> LocalBlobStore {
        LocalBlobStore { root: root.into() }
    }
}

impl BlobStore for LocalBlobStore {
    fn put(&self, name: &str, bytes: &[u8]) -> AnyhowResult<()> {
        let path = self.root.join(name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, bytes)?;
        Ok(())
    }

    fn get(&self, name: &str) -> AnyhowResult<Option<Vec<u8>>> {
        match std::fs::read(self.root.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn get_range(&self, name: &str, start: u64, len: u64) -> AnyhowResult<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = std::fs::File::open(self.root.join(name))?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; usize::try_from(len)?];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

/// One occurrence within a line: byte offsets into `LineEvent.text`,
/// half-open, clamped to the line's content (pre-`\n`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubMatch {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Match,
    Context,
}

/// One output line of a search: a matching line or a context line around
/// one. The owning object's key travels alongside, not inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEvent {
    /// 1-based line number.
    pub line: u64,
    pub kind: LineKind,
    /// Byte offset of the line start in the decoded doc.
    pub offset: u64,
    /// Exact line bytes INCLUDING the trailing `\n` when present.
    pub text: Vec<u8>,
    /// Ordered by start; non-empty for Match. A Context line past a
    /// `max_count` cap can also carry submatches (rg behavior).
    pub submatches: Vec<SubMatch>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MatchOptions {
    pub before_context: usize,
    pub after_context: usize,
    /// Cap on MATCHING lines per doc (`rg -m`). After-context still drains.
    pub max_count: Option<u64>,
}

/// Run `re` over one decoded doc, producing the rg-ordered, overlap-merged
/// line event stream: events sorted by line, each line present at most once,
/// matches preferred over context. Empty result == zero matching lines.
pub fn grep_doc(bytes: &[u8], re: &regex::bytes::Regex, options: MatchOptions) -> Vec<LineEvent> {
    if options.max_count == Some(0) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut finder = re.find_iter(bytes).peekable();
    let mut ring: VecDeque<(u64, usize, usize)> = VecDeque::new();
    let mut line_no: u64 = 0;
    let mut pos = 0usize;
    let mut last_emitted: u64 = 0;
    let mut after_remaining = 0usize;
    let mut matched_lines: u64 = 0;
    let mut done = false;
    while pos < bytes.len() {
        line_no += 1;
        let (content_end, span_end) = match memchr::memchr(b'\n', &bytes[pos..]) {
            Some(off) => (pos + off, pos + off + 1),
            None => (bytes.len(), bytes.len()),
        };
        let mut subs = Vec::new();
        while finder.peek().is_some_and(|m| m.start() < span_end) {
            let m = finder.next().expect("peeked");
            subs.push(SubMatch {
                start: m.start() - pos,
                end: m.end().min(content_end).max(m.start()) - pos,
            });
        }
        if !subs.is_empty() && !done {
            while let Some((l, s, e)) = ring.pop_front() {
                if l <= last_emitted {
                    continue;
                }
                out.push(LineEvent {
                    line: l,
                    kind: LineKind::Context,
                    offset: s as u64,
                    text: bytes[s..e].to_vec(),
                    submatches: Vec::new(),
                });
            }
            out.push(LineEvent {
                line: line_no,
                kind: LineKind::Match,
                offset: pos as u64,
                text: bytes[pos..span_end].to_vec(),
                submatches: subs,
            });
            last_emitted = line_no;
            matched_lines += 1;
            after_remaining = options.after_context;
            if options.max_count == Some(matched_lines) {
                done = true;
            }
        } else if after_remaining > 0 {
            out.push(LineEvent {
                line: line_no,
                kind: LineKind::Context,
                offset: pos as u64,
                text: bytes[pos..span_end].to_vec(),
                submatches: subs,
            });
            last_emitted = line_no;
            after_remaining -= 1;
        } else if options.before_context > 0 {
            if ring.len() == options.before_context {
                ring.pop_front();
            }
            ring.push_back((line_no, pos, span_end));
        }
        if (done || finder.peek().is_none()) && after_remaining == 0 {
            break;
        }
        pos = span_end;
    }
    out
}

/// Line-semantics match test (rg behavior): a doc matches iff some match
/// STARTS before EOF — an empty doc has no lines and never matches, and an
/// empty match at EOF belongs to no line.
pub fn has_line_match(bytes: &[u8], re: &regex::bytes::Regex) -> bool {
    re.find(bytes).is_some_and(|m| m.start() < bytes.len())
}

/// Oracle: keys of docs containing at least one matching line, sorted. The
/// differential ground truth.
pub fn scan_matching_docs(
    corpus: &dyn Corpus,
    re: &regex::bytes::Regex,
) -> AnyhowResult<Vec<String>> {
    let mut hits = Vec::new();
    for (id, key) in corpus.docs() {
        let bytes = corpus.fetch(*id)?;
        if has_line_match(&bytes, re) {
            hits.push(key.clone());
        }
    }
    hits.sort_unstable();
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn trigrams_basic() {
        // "abcab" -> abc, bca, cab sorted; "abc" appears once after dedup
        assert_eq!(
            trigram_grams_bytes(b"abcab"),
            vec![b"abc".to_vec(), b"bca".to_vec(), b"cab".to_vec()]
        );
    }

    #[test]
    fn trigrams_short_is_empty() {
        assert!(trigram_grams_bytes(b"ab").is_empty());
        assert!(trigram_grams_bytes(b"").is_empty());
    }

    #[test]
    fn trigram_query_subset_of_index() {
        use std::collections::HashSet;
        let pattern = b"CONSTANT";
        let content = b"let CONSTANT = 1;";
        let all: HashSet<Vec<u8>> = grams_index(content, Strategy::Trigram)
            .into_iter()
            .collect();
        let q: HashSet<Vec<u8>> = grams_query(pattern, Strategy::Trigram)
            .into_iter()
            .collect();
        assert!(q.is_subset(&all));
    }

    #[test]
    fn local_blob_store_round_trips_ranges() -> AnyhowResult<()> {
        let root = std::env::temp_dir().join(format!(
            "holys3-core-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let store = LocalBlobStore::new(&root);
        store.put("builds/a/postings.bin", b"abcdef")?;
        assert_eq!(
            store.get("builds/a/postings.bin")?.as_deref(),
            Some(b"abcdef".as_slice())
        );
        assert_eq!(store.get("missing")?, None);
        assert_eq!(store.get_range("builds/a/postings.bin", 2, 3)?, b"cde");
        assert_eq!(
            store.get_ranges("builds/a/postings.bin", &[(0, 2), (4, 2)])?,
            vec![b"ab".to_vec(), b"ef".to_vec()]
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}

#[cfg(test)]
mod sparse_tests {
    use super::invariant_grams::{extract_sparse_ngrams_all, extract_sparse_ngrams_covering};
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn sparse_short_input() {
        assert!(extract_sparse_ngrams_all(b"a").is_empty());
        assert!(!extract_sparse_ngrams_all(b"ab").is_empty());
        assert!(extract_sparse_ngrams_covering(b"a").is_empty());
        assert!(!extract_sparse_ngrams_covering(b"ab").is_empty());
    }

    #[test]
    fn covering_subset_of_all_same_input() {
        let input = b"MAX_FILE_SIZE";
        let all: HashSet<u64> = extract_sparse_ngrams_all(input)
            .iter()
            .map(|(h, _)| *h)
            .collect();
        let cov: HashSet<u64> = extract_sparse_ngrams_covering(input)
            .iter()
            .map(|(h, _)| *h)
            .collect();
        assert!(cov.is_subset(&all));
        assert!(all.len() >= cov.len());
    }

    #[test]
    fn subset_invariant_modified_constant() {
        // covering(pattern) must be a subset of all(content) when pattern occurs in content.
        let pattern = b"MODIFIED_CONSTANT";
        let content = b"fn main() {\n let x = MODIFIED_CONSTANT;\n}\n";
        let all: HashSet<u64> = extract_sparse_ngrams_all(content)
            .iter()
            .map(|(h, _)| *h)
            .collect();
        let cov: HashSet<u64> = extract_sparse_ngrams_covering(pattern)
            .iter()
            .map(|(h, _)| *h)
            .collect();
        let missing: Vec<u64> = cov.difference(&all).copied().collect();
        assert!(
            missing.is_empty(),
            "covering(pattern) must be subset of all(content); missing: {missing:?}"
        );
    }

    #[test]
    fn covering_bytes_subset_of_all_bytes() {
        let pattern = b"MODIFIED_CONSTANT";
        let content = b"fn main() {\n let x = MODIFIED_CONSTANT;\n}\n";
        let all: HashSet<Vec<u8>> = sparse_grams_all_bytes(content).into_iter().collect();
        let cov: HashSet<Vec<u8>> = sparse_grams_covering_bytes(pattern).into_iter().collect();
        assert!(
            cov.is_subset(&all),
            "covering bytes must be subset of all bytes"
        );
    }

    #[test]
    fn subset_invariant_randomized() {
        // Deterministic pseudo-random fuzz of the invariant across many embeddings.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200 {
            let plen = 2 + (next() % 12) as usize;
            let pattern: Vec<u8> = (0..plen).map(|_| (next() % 96 + 32) as u8).collect();
            let pre: Vec<u8> = (0..(next() % 8) as usize)
                .map(|_| (next() % 96 + 32) as u8)
                .collect();
            let post: Vec<u8> = (0..(next() % 8) as usize)
                .map(|_| (next() % 96 + 32) as u8)
                .collect();
            let mut content = pre.clone();
            content.extend_from_slice(&pattern);
            content.extend_from_slice(&post);
            let all: HashSet<u64> = extract_sparse_ngrams_all(&content)
                .iter()
                .map(|(h, _)| *h)
                .collect();
            let cov: HashSet<u64> = extract_sparse_ngrams_covering(&pattern)
                .iter()
                .map(|(h, _)| *h)
                .collect();
            assert!(
                cov.is_subset(&all),
                "invariant broke for pattern {pattern:?} in content {content:?}"
            );
        }
    }
}

#[cfg(test)]
mod corpus_tests {
    use super::*;
    use crate::testutil::MemCorpus;

    #[test]
    fn scan_finds_matching_docs() {
        let c = MemCorpus::new(
            vec![(0, "a".into()), (1, "b".into())],
            vec![b"hello world".to_vec(), b"nothing here".to_vec()],
        );
        let re = regex::bytes::Regex::new("world").unwrap();
        assert_eq!(scan_matching_docs(&c, &re).unwrap(), vec!["a".to_owned()]);
    }

    fn re(p: &str) -> regex::bytes::Regex {
        regex::bytes::Regex::new(p).unwrap()
    }

    type EventShape = (u64, LineKind, Vec<(usize, usize)>);

    fn shape(events: &[LineEvent]) -> Vec<EventShape> {
        events
            .iter()
            .map(|e| {
                (
                    e.line,
                    e.kind,
                    e.submatches.iter().map(|s| (s.start, s.end)).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn match_line_col() {
        let events = grep_doc(b"foo\nbar baz", &re("baz"), MatchOptions::default());
        assert_eq!(
            events,
            vec![LineEvent {
                line: 2,
                kind: LineKind::Match,
                offset: 4,
                text: b"bar baz".to_vec(),
                submatches: vec![SubMatch { start: 4, end: 7 }],
            }]
        );
    }

    #[test]
    fn grep_doc_merges_per_line_and_tracks_lines() {
        // x appears twice on line 3: ONE event with two submatches
        let bytes = b"alpha x\nbeta\nx gamma x\nx";
        let events = grep_doc(bytes, &re("x"), MatchOptions::default());
        assert_eq!(
            shape(&events),
            vec![
                (1, LineKind::Match, vec![(6, 7)]),
                (3, LineKind::Match, vec![(0, 1), (8, 9)]),
                (4, LineKind::Match, vec![(0, 1)]),
            ]
        );
        assert_eq!(events[1].text, b"x gamma x\n".to_vec());
        assert_eq!(events[2].text, b"x".to_vec());
    }

    #[test]
    fn grep_doc_context_merges_overlaps() {
        // matches on lines 3 and 5 with C=2: lines 1-7 once each, 3+5 Match
        let bytes = b"l1\nl2\nhit\nl4\nhit\nl6\nl7\nl8\n";
        let opts = MatchOptions {
            before_context: 2,
            after_context: 2,
            max_count: None,
        };
        let events = grep_doc(bytes, &re("hit"), opts);
        let lines: Vec<(u64, LineKind)> = events.iter().map(|e| (e.line, e.kind)).collect();
        assert_eq!(
            lines,
            vec![
                (1, LineKind::Context),
                (2, LineKind::Context),
                (3, LineKind::Match),
                (4, LineKind::Context),
                (5, LineKind::Match),
                (6, LineKind::Context),
                (7, LineKind::Context),
            ]
        );
    }

    #[test]
    fn grep_doc_independent_before_after() {
        let bytes = b"a\nb\nhit\nc\nd\n";
        let only_after = MatchOptions {
            after_context: 1,
            ..Default::default()
        };
        let events = grep_doc(bytes, &re("hit"), only_after);
        assert_eq!(
            events.iter().map(|e| e.line).collect::<Vec<_>>(),
            vec![3, 4]
        );
        let only_before = MatchOptions {
            before_context: 1,
            ..Default::default()
        };
        let events = grep_doc(bytes, &re("hit"), only_before);
        assert_eq!(
            events.iter().map(|e| e.line).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn grep_doc_max_count_caps_but_drains_after_context() {
        let bytes = b"hit\nmid\nhit\ntail\n";
        let opts = MatchOptions {
            after_context: 1,
            max_count: Some(1),
            ..Default::default()
        };
        let events = grep_doc(bytes, &re("hit"), opts);
        // one Match, then line 2 as after-context; the capped line-3 match
        // never surfaces because after-context ran out before it
        assert_eq!(
            shape(&events),
            vec![
                (1, LineKind::Match, vec![(0, 3)]),
                (2, LineKind::Context, vec![]),
            ]
        );
        assert!(grep_doc(
            bytes,
            &re("hit"),
            MatchOptions {
                max_count: Some(0),
                ..Default::default()
            }
        )
        .is_empty());
    }

    #[test]
    fn grep_doc_post_cap_match_in_context_carries_submatches() {
        let bytes = b"hit\nhit\nrest\n";
        let opts = MatchOptions {
            after_context: 1,
            max_count: Some(1),
            ..Default::default()
        };
        let events = grep_doc(bytes, &re("hit"), opts);
        assert_eq!(
            shape(&events),
            vec![
                (1, LineKind::Match, vec![(0, 3)]),
                (2, LineKind::Context, vec![(0, 3)]),
            ]
        );
    }

    #[test]
    fn grep_doc_eof_line_without_newline() {
        let events = grep_doc(b"no newline tail", &re("tail"), MatchOptions::default());
        assert_eq!(events[0].text, b"no newline tail".to_vec());
        assert_eq!(events[0].submatches, vec![SubMatch { start: 11, end: 15 }]);
    }

    #[test]
    fn decode_body_handles_raw_gzip_multimember_and_zstd() {
        use std::io::Write;

        assert_eq!(
            decode_body("k", b"plain text".to_vec()).unwrap(),
            b"plain text"
        );

        let gz = |data: &[u8]| {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(data).unwrap();
            enc.finish().unwrap()
        };
        let mut multi = gz(b"first member\n");
        multi.extend(gz(b"second member\n"));
        assert_eq!(
            decode_body("k.gz", multi).unwrap(),
            b"first member\nsecond member\n"
        );

        let zst = zstd::stream::encode_all(&b"zstd body"[..], 0).unwrap();
        assert_eq!(decode_body("k.zst", zst).unwrap(), b"zstd body");

        let truncated = gz(b"data")[..6].to_vec();
        let err = decode_body("bad.gz", truncated).unwrap_err();
        assert!(err.to_string().contains("bad.gz"));
    }

    #[test]
    fn doc_fetcher_resolves_keys() {
        use crate::testutil::MemCorpus;
        use crate::DocFetcher;
        let c = MemCorpus::new(
            vec![(0, "a".into()), (1, "b".into())],
            vec![b"one".to_vec(), b"two".to_vec()],
        );
        let keys = vec!["b".to_owned(), "a".to_owned()];
        let mut seen = Vec::new();
        c.fetch_each(&keys, &mut |idx, bytes| {
            seen.push((idx, bytes));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![(0, b"two".to_vec()), (1, b"one".to_vec())]);
    }

    #[test]
    fn fetch_many_aborts_on_first_error() {
        struct BrokenCorpus {
            docs: Vec<(DocId, String)>,
        }

        impl Corpus for BrokenCorpus {
            fn docs(&self) -> &[(DocId, String)] {
                &self.docs
            }

            fn fetch(&self, id: DocId) -> AnyhowResult<Vec<u8>> {
                if id == 1 {
                    anyhow::bail!("broken");
                }
                Ok(b"ok".to_vec())
            }
        }

        let corpus = BrokenCorpus {
            docs: vec![(0, "a".into()), (1, "b".into())],
        };
        assert!(corpus.fetch_many(&[0, 1]).is_err());
    }
}
