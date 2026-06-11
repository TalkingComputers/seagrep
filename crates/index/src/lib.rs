#![cfg_attr(docsrs, feature(doc_auto_cfg))]
//! Index construction and local or store-backed index readers.

mod eval;
mod search;
mod segment;

pub use search::{
    search_collect, search_streaming, DocResult, KeyScope, MatchSink, NullSink, SinkFlow,
};
pub use segment::{update_index, CorpusFactory, SegmentedReader, UpdateReport};

use anyhow::{Context, Result};
use eval::Selection;
use holys3_core::{decode_body, grams_index, hash_ngram, Corpus, DocFetcher, DocId, Strategy};
use holys3_query::Query;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Docs are fetched and gram-extracted in chunks of this many, bounding build
/// memory to one chunk of bodies instead of the whole corpus.
const BUILD_FETCH_CHUNK: usize = 1024;

/// Bumped whenever index semantics change (e.g. grams now cover decompressed
/// bodies); an index built by an older holys3 must error, not silently
/// return wrong results.
const INDEX_FORMAT: u32 = 4;

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    build_id: String,
    strategy: Strategy,
    terms_fst_len: u64,
    postings_len: u64,
    docs: Vec<(DocId, String)>,
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    let manifest: Manifest = postcard::from_bytes(bytes)
        .context("index manifest unreadable; run `holys3 index` to rebuild")?;
    anyhow::ensure!(
        manifest.format == INDEX_FORMAT,
        "index format {} is not the current {INDEX_FORMAT}; run `holys3 index` to rebuild",
        manifest.format
    );
    Ok(manifest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    pub distinct_grams: u64,
    pub terms_fst_bytes: u64,
    pub postings_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchStats {
    /// Sorted keys of docs with at least one verified match.
    pub hits: Vec<String>,
    pub candidates: usize,
    pub total_docs: usize,
    pub bytes_fetched: usize,
}

pub trait IndexReader {
    fn strategy(&self) -> Strategy;
    fn total_docs(&self) -> usize;
    /// Candidate object keys: a superset of the docs that can match.
    /// `key_prefix` lets implementations prune whole index regions before
    /// any fetch; the engine re-applies it per key regardless.
    fn candidate_keys(&self, q: &Query, key_prefix: Option<&str>) -> Result<Vec<String>>;
    fn stats(&self) -> IndexStats;
}

/// Map candidate local ids to keys, applying the prefix filter.
fn ids_to_keys(docs: &[(DocId, String)], ids: Vec<DocId>, key_prefix: Option<&str>) -> Vec<String> {
    ids.into_iter()
        .map(|id| docs[id as usize].1.clone())
        .filter(|key| key_prefix.is_none_or(|prefix| key.starts_with(prefix)))
        .collect()
}

/// Bit width of one stored doc id: just wide enough for the largest id in
/// `0..doc_count`. A pure function of `doc_count`, so block byte lengths
/// stay derivable BEFORE any fetch.
fn posting_id_bits(doc_count: u32) -> u32 {
    (32 - doc_count.saturating_sub(1).leading_zeros()).max(1)
}

/// How many ids a block physically stores: the COMPLEMENT (absent ids) when
/// the gram is in more than half the docs, the present ids otherwise, and
/// nothing at all when the gram is in every doc. The representation class is
/// a pure function of `(count, doc_count)` — no flags, no sniffing.
fn stored_id_count(count: u32, doc_count: u32) -> u64 {
    if count == doc_count {
        0
    } else if u64::from(count) * 2 > u64::from(doc_count) {
        // saturating: a corrupt count > doc_count yields 0 here and is then
        // rejected loudly by decode_posting_block's count <= doc_count check
        u64::from(doc_count.saturating_sub(count))
    } else {
        u64::from(count)
    }
}

/// On-disk byte length of a posting block: `stored_id_count` ids bit-packed
/// at `posting_id_bits` each, rounded up to whole bytes.
pub(crate) fn posting_block_len(count: u32, doc_count: u32) -> u64 {
    let bits = stored_id_count(count, doc_count) * u64::from(posting_id_bits(doc_count));
    bits.div_ceil(8)
}

fn pack_ids(buf: &mut Vec<u8>, ids: impl Iterator<Item = DocId>, width: u32) {
    let mut acc: u64 = 0;
    let mut filled: u32 = 0;
    for id in ids {
        acc |= u64::from(id) << filled;
        filled += width;
        while filled >= 8 {
            buf.push(acc as u8);
            acc >>= 8;
            filled -= 8;
        }
    }
    if filled > 0 {
        buf.push(acc as u8);
    }
}

fn unpack_ids(bytes: &[u8], n: u64, width: u32) -> Vec<DocId> {
    let mut out = Vec::with_capacity(usize::try_from(n).unwrap_or(0));
    let mut acc: u64 = 0;
    let mut filled: u32 = 0;
    let mut input = bytes.iter();
    let mask: u64 = (1u64 << width) - 1;
    for _ in 0..n {
        while filled < width {
            acc |= u64::from(*input.next().expect("length validated")) << filled;
            filled += 8;
        }
        out.push((acc & mask) as u32);
        acc >>= width;
        filled -= width;
    }
    out
}

fn encode_posting_block(buf: &mut Vec<u8>, ids: &[DocId], doc_count: u32) {
    let count = ids.len() as u32;
    if count == doc_count {
        return;
    }
    let width = posting_id_bits(doc_count);
    if u64::from(count) * 2 > u64::from(doc_count) {
        let mut present = ids.iter().copied().peekable();
        let absent = (0..doc_count).filter(|id| {
            if present.peek() == Some(id) {
                present.next();
                false
            } else {
                true
            }
        });
        pack_ids(buf, absent, width);
    } else {
        pack_ids(buf, ids.iter().copied(), width);
    }
}

/// Inverse of `encode_posting_block`. Validates exact length, strict
/// ascending order, and id bounds — a block that fails any of these is a
/// corrupt index, reported loudly.
pub(crate) fn decode_posting_block(bytes: &[u8], count: u32, doc_count: u32) -> Result<Vec<DocId>> {
    anyhow::ensure!(
        count <= doc_count,
        "posting count {count} exceeds doc count {doc_count}"
    );
    let expected = posting_block_len(count, doc_count);
    anyhow::ensure!(
        bytes.len() as u64 == expected,
        "posting block is {} bytes, expected {expected}",
        bytes.len()
    );
    if count == doc_count {
        return Ok((0..doc_count).collect());
    }
    let stored = unpack_ids(
        bytes,
        stored_id_count(count, doc_count),
        posting_id_bits(doc_count),
    );
    for pair in stored.windows(2) {
        anyhow::ensure!(
            pair[0] < pair[1],
            "posting block ids are not strictly ascending"
        );
    }
    if let Some(&last) = stored.last() {
        anyhow::ensure!(
            last < doc_count,
            "posting block references doc {last} >= doc_count {doc_count}"
        );
    }
    if u64::from(count) * 2 > u64::from(doc_count) {
        let mut absent = stored.into_iter().peekable();
        let mut present = Vec::with_capacity(count as usize);
        for id in 0..doc_count {
            if absent.peek() == Some(&id) {
                absent.next();
            } else {
                present.push(id);
            }
        }
        Ok(present)
    } else {
        Ok(stored)
    }
}

/// Shared candidates pipeline: resolve grams against the term dict (no IO),
/// fetch every needed posting block via `fetch_blocks`, evaluate purely.
/// Returns local ids in `0..doc_count`.
pub(crate) fn candidates_with<D: AsRef<[u8]>>(
    map: &fst::Map<D>,
    doc_count: u32,
    q: &Query,
    fetch_blocks: impl FnOnce(&BTreeMap<u64, u32>) -> Result<BTreeMap<u64, Vec<DocId>>>,
) -> Result<Vec<DocId>> {
    let resolved = eval::resolve(q, doc_count, &|gram| map.get(gram));
    let mut needed = BTreeMap::new();
    eval::blocks_needed(&resolved, &mut needed);
    let blocks = fetch_blocks(&needed)?;
    match eval::eval(&resolved, &blocks)? {
        Selection::All => Ok((0..doc_count).collect()),
        Selection::Ids(ids) => Ok(ids),
    }
}

/// Build terms.fst + postings.bin over the corpus. Also returns the ids of
/// docs that contributed NO grams because they vanished mid-build (404) or
/// failed to decompress — segment writers tombstone their etags so the next
/// incremental run retries them.
fn build_index_bytes(
    corpus: &dyn Corpus,
    strategy: Strategy,
) -> Result<(Vec<u8>, Vec<u8>, Vec<DocId>)> {
    let mut postings: BTreeMap<Vec<u8>, Vec<DocId>> = BTreeMap::new();
    let doc_keys = corpus.docs();
    let ids = doc_keys.iter().map(|&(id, _)| id).collect::<Vec<_>>();
    let mut ungrammed: Vec<DocId> = Vec::new();
    for chunk in ids.chunks(BUILD_FETCH_CHUNK) {
        let fetched = corpus.fetch_many(chunk)?;
        let mut seen = vec![false; chunk.len()];
        let base = chunk[0];
        let docs = fetched
            .into_par_iter()
            .filter_map(
                |(id, bytes)| match decode_body(&doc_keys[id as usize].1, bytes) {
                    Ok(text) => Some((id, grams_index(&text, strategy))),
                    Err(err) => {
                        eprintln!("warning: {err:#}; object excluded from index");
                        None
                    }
                },
            )
            .collect::<Vec<_>>();
        for (id, _) in &docs {
            seen[(id - base) as usize] = true;
        }
        ungrammed.extend(
            chunk
                .iter()
                .zip(&seen)
                .filter(|(_, seen)| !**seen)
                .map(|(&id, _)| id),
        );
        for (id, grams) in docs {
            for gram in grams {
                postings.entry(gram).or_default().push(id);
            }
        }
    }
    if !ungrammed.is_empty() {
        eprintln!(
            "warning: {} objects vanished or could not be decompressed and were excluded",
            ungrammed.len()
        );
    }
    let (fst_bytes, postings_buf) = serialize_postings(postings, doc_keys.len() as u32)?;
    ungrammed.sort_unstable();
    Ok((fst_bytes, postings_buf, ungrammed))
}

/// THE postings format: per gram, a density-classed block in postings.bin
/// (see `posting_block_len`); the fst maps gram -> packed (offset, count).
/// Shared by fresh builds and compaction merges so the format is defined
/// once. Dense grams cost zero bytes — the query path never fetches them
/// (`resolve` short-circuits them to ALL) and decode reconstructs them.
pub(crate) fn serialize_postings(
    postings: BTreeMap<Vec<u8>, Vec<DocId>>,
    doc_count: u32,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut postings_buf: Vec<u8> = Vec::new();
    let mut builder = fst::MapBuilder::new(Vec::new())?;
    for (gram, mut ids) in postings {
        ids.sort_unstable();
        ids.dedup();
        // A gram whose docs all died (compaction) must be ABSENT, not empty:
        // a zero-length block shares its offset with the next block, and the
        // offset-keyed fetch map would clobber the neighbor's count.
        if ids.is_empty() {
            continue;
        }
        let offset = postings_buf.len() as u64;
        encode_posting_block(&mut postings_buf, &ids, doc_count);
        builder.insert(gram, eval::pack_posting(offset, ids.len())?)?;
    }
    Ok((builder.into_inner()?, postings_buf))
}

fn make_manifest(
    corpus: &dyn Corpus,
    strategy: Strategy,
    build_id: &str,
    fst_bytes: &[u8],
    postings_buf: &[u8],
) -> Result<Manifest> {
    Ok(Manifest {
        format: INDEX_FORMAT,
        build_id: build_id.to_owned(),
        strategy,
        terms_fst_len: u64::try_from(fst_bytes.len())?,
        postings_len: u64::try_from(postings_buf.len())?,
        docs: corpus.docs().to_vec(),
    })
}

/// Write terms.fst + postings.bin + manifest.bin into `dir`.
pub fn build_to_dir(corpus: &dyn Corpus, dir: &Path, strategy: Strategy) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let (fst_bytes, postings_buf, _) = build_index_bytes(corpus, strategy)?;
    let ids = corpus
        .docs()
        .iter()
        .map(|(_, key)| (key.clone(), String::new()))
        .collect::<Vec<_>>();
    let manifest = make_manifest(
        corpus,
        strategy,
        &compute_build_id(&ids, strategy),
        &fst_bytes,
        &postings_buf,
    )?;
    std::fs::write(dir.join("terms.fst"), &fst_bytes)?;
    std::fs::write(dir.join("postings.bin"), &postings_buf)?;
    std::fs::write(dir.join("manifest.bin"), postcard::to_allocvec(&manifest)?)?;
    Ok(())
}

/// Content-addressed build id. Includes the strategy and index format so a
/// rebuild of the same corpus with different settings can never collide with
/// (and silently mix into) a cached build under `builds/<id>`.
fn compute_build_id(objects: &[(String, String)], strategy: Strategy) -> String {
    let mut objects = objects.iter().collect::<Vec<_>>();
    objects.sort_unstable();
    let mut bytes = format!("format={INDEX_FORMAT};strategy={strategy:?}\n").into_bytes();
    for (key, etag) in objects {
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(etag.as_bytes());
        bytes.push(b'\n');
    }
    format!("{:016x}", hash_ngram(&bytes))
}

pub struct MmapIndexReader {
    map: fst::Map<memmap2::Mmap>,
    postings: memmap2::Mmap,
    docs: Vec<(DocId, String)>,
    strategy: Strategy,
}

impl MmapIndexReader {
    pub fn open(dir: &Path) -> Result<MmapIndexReader> {
        let fst_file = std::fs::File::open(dir.join("terms.fst"))?;
        let map = fst::Map::new(unsafe {
            // Build dirs are immutable while readers are open.
            memmap2::Mmap::map(&fst_file)?
        })?;
        let post_file = std::fs::File::open(dir.join("postings.bin"))?;
        let postings = unsafe {
            // Build dirs are immutable while readers are open.
            memmap2::Mmap::map(&post_file)?
        };
        let manifest = parse_manifest(&std::fs::read(dir.join("manifest.bin"))?)?;
        Ok(MmapIndexReader {
            map,
            postings,
            docs: manifest.docs,
            strategy: manifest.strategy,
        })
    }
}

impl MmapIndexReader {
    /// All doc keys in this index (id order). Local-mode helper.
    pub fn doc_keys(&self) -> impl Iterator<Item = &str> {
        self.docs.iter().map(|(_, key)| key.as_str())
    }
}

impl IndexReader for MmapIndexReader {
    fn strategy(&self) -> Strategy {
        self.strategy
    }

    fn total_docs(&self) -> usize {
        self.docs.len()
    }

    fn candidate_keys(&self, q: &Query, key_prefix: Option<&str>) -> Result<Vec<String>> {
        let doc_count = self.docs.len() as u32;
        let ids = candidates_with(&self.map, doc_count, q, |needed| {
            needed
                .iter()
                .map(|(&offset, &count)| {
                    let start = usize::try_from(offset)?;
                    let end = start
                        .checked_add(usize::try_from(posting_block_len(count, doc_count))?)
                        .context("posting block end overflow")?;
                    let bytes = self
                        .postings
                        .get(start..end)
                        .context("truncated postings.bin")?;
                    Ok((offset, decode_posting_block(bytes, count, doc_count)?))
                })
                .collect()
        })?;
        Ok(ids_to_keys(&self.docs, ids, key_prefix))
    }

    fn stats(&self) -> IndexStats {
        IndexStats {
            distinct_grams: self.map.len() as u64,
            terms_fst_bytes: self.map.as_fst().as_bytes().len() as u64,
            postings_bytes: self.postings.len() as u64,
        }
    }
}

pub struct LocalCorpus {
    docs: Vec<(DocId, String)>,
    paths: Vec<PathBuf>,
}

impl LocalCorpus {
    /// Walk `root` recursively. Symlinks are skipped, so cycles cannot hang
    /// the walk.
    pub fn new(root: &Path) -> Result<LocalCorpus> {
        let mut paths = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            for entry in std::fs::read_dir(&p)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    stack.push(entry.path());
                } else if file_type.is_file() {
                    paths.push(entry.path());
                }
            }
        }
        paths.sort();
        // Keys are S3-style `/`-separated on every platform so globs,
        // prefixes, and key-time scoping behave identically; Windows file
        // APIs accept forward slashes, so fetching by key still works.
        #[cfg(windows)]
        let key_of = |p: &PathBuf| p.to_string_lossy().replace('\\', "/");
        #[cfg(not(windows))]
        let key_of = |p: &PathBuf| p.to_string_lossy().into_owned();
        let docs = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (i as DocId, key_of(p)))
            .collect();
        Ok(LocalCorpus { docs, paths })
    }
}

impl Corpus for LocalCorpus {
    fn docs(&self) -> &[(DocId, String)] {
        &self.docs
    }

    fn fetch(&self, id: DocId) -> Result<Vec<u8>> {
        Ok(std::fs::read(&self.paths[id as usize])?)
    }
}

/// Search-side fetch for local files: the key IS the path.
pub struct LocalFetcher;

impl DocFetcher for LocalFetcher {
    fn fetch_each(
        &self,
        keys: &[String],
        consume: &mut dyn FnMut(usize, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        for (idx, key) in keys.iter().enumerate() {
            consume(idx, std::fs::read(key)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holys3_core::testutil::MemCorpus;
    use holys3_core::{LineEvent, LineKind, MatchOptions, SubMatch};

    fn build_tmp(c: &MemCorpus, strategy: Strategy) -> (tempfile::TempDir, MmapIndexReader) {
        let dir = tempfile::tempdir().unwrap();
        build_to_dir(c, dir.path(), strategy).unwrap();
        let r = MmapIndexReader::open(dir.path()).unwrap();
        (dir, r)
    }

    #[test]
    fn posting_blocks_round_trip_every_density_class() {
        let cases: Vec<(Vec<u32>, u32)> = vec![
            (vec![], 10),             // empty list
            (vec![3], 10),            // single
            (vec![0, 1, 2], 7),       // sparse (below half)
            ((0..5).collect(), 10),   // exactly half: stored as ids
            ((0..6).collect(), 10),   // just over half: complement
            ((0..9).collect(), 10),   // doc_count - 1: complement of one
            ((0..10).collect(), 10),  // fully dense: zero bytes
            (vec![0], 1),             // doc_count = 1, dense
            (vec![1, 3, 5, 7], 8),    // exactly half at even doc_count
            (vec![0, 2, 4, 6, 7], 8), // over half
        ];
        for (ids, doc_count) in cases {
            let mut buf = Vec::new();
            encode_posting_block(&mut buf, &ids, doc_count);
            assert_eq!(
                buf.len() as u64,
                posting_block_len(ids.len() as u32, doc_count),
                "len mismatch for {ids:?}/{doc_count}"
            );
            let decoded = decode_posting_block(&buf, ids.len() as u32, doc_count).unwrap();
            assert_eq!(decoded, ids, "round trip failed for doc_count {doc_count}");
        }
        // dense stores nothing
        let mut buf = Vec::new();
        encode_posting_block(&mut buf, &(0..10).collect::<Vec<_>>(), 10);
        assert!(buf.is_empty());
    }

    #[test]
    fn posting_block_decode_rejects_corruption() {
        // wrong length
        assert!(decode_posting_block(&[0, 0, 0, 0], 2, 10).is_err());
        // count above doc_count
        assert!(decode_posting_block(&[], 11, 10).is_err());
        // out-of-bounds id (sparse class: 1 of 10 -> 4 bytes)
        assert!(decode_posting_block(&10u32.to_le_bytes(), 1, 10).is_err());
        // unsorted ids (2 of 10 -> stored as ids)
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        assert!(decode_posting_block(&buf, 2, 10).is_err());
        // duplicate ids are not strictly ascending
        let mut buf = Vec::new();
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        assert!(decode_posting_block(&buf, 2, 10).is_err());
    }

    #[test]
    fn candidate_superset_then_verify() {
        let c = MemCorpus::new(
            vec![(0, "x".into()), (1, "y".into())],
            vec![b"world".to_vec(), b"word".to_vec()],
        );
        for strategy in [Strategy::Trigram, Strategy::Sparse] {
            let (_d, r) = build_tmp(&c, strategy);
            let cands = r
                .candidate_keys(&holys3_query::plan("world", r.strategy()).unwrap(), None)
                .unwrap();
            assert!(cands.iter().any(|key| key == "x"));
            assert!(cands.iter().all(|key| key == "x" || key == "y"));
        }
    }

    #[test]
    fn all_returns_every_doc() {
        let c = MemCorpus::new(vec![(0, "x".into())], vec![b"abcdef".to_vec()]);
        for strategy in [Strategy::Trigram, Strategy::Sparse] {
            let (_d, r) = build_tmp(&c, strategy);
            assert_eq!(r.candidate_keys(&Query::All, None).unwrap(), vec!["x"]);
        }
    }

    #[test]
    fn search_collect_returns_verified_matches_and_stats() {
        let c = MemCorpus::new(
            vec![(0, "x".into()), (1, "y".into())],
            vec![b"abc world".to_vec(), b"nomatch".to_vec()],
        );
        let (_d, r) = build_tmp(&c, Strategy::Trigram);
        let (matches, stats) = search_collect(&r, &c, "world").unwrap();
        assert_eq!(
            matches,
            vec![(
                "x".to_owned(),
                LineEvent {
                    line: 1,
                    kind: LineKind::Match,
                    offset: 0,
                    text: b"abc world".to_vec(),
                    submatches: vec![SubMatch { start: 4, end: 9 }],
                }
            )]
        );
        assert_eq!(stats.hits, vec!["x"]);
        assert_eq!(stats.candidates, 1);
        assert_eq!(stats.total_docs, 2);
        assert_eq!(stats.bytes_fetched, b"abc world".len());
    }

    #[test]
    fn files_only_streaming_matches_full_search() {
        let c = MemCorpus::new(
            vec![(0, "x".into()), (1, "y".into()), (2, "z".into())],
            vec![
                b"abc world".to_vec(),
                b"nomatch".to_vec(),
                b"world world".to_vec(),
            ],
        );
        let (_d, r) = build_tmp(&c, Strategy::Trigram);
        let stats = search_streaming(
            &r,
            &c,
            "world",
            KeyScope::default(),
            MatchOptions::default(),
            &NullSink,
        )
        .unwrap();
        let (_, full_stats) = search_collect(&r, &c, "world").unwrap();
        assert_eq!(stats.hits, full_stats.hits);
        assert_eq!(stats.hits, vec!["x", "z"]);
    }

    #[test]
    fn key_filter_prunes_before_fetch() {
        let c = MemCorpus::new(
            vec![(0, "logs/a".into()), (1, "other/b".into())],
            vec![b"abc world".to_vec(), b"abc world".to_vec()],
        );
        let (_d, r) = build_tmp(&c, Strategy::Trigram);
        let scope = KeyScope {
            prefix: Some("logs/"),
            matches: None,
        };
        let stats =
            search_streaming(&r, &c, "world", scope, MatchOptions::default(), &NullSink).unwrap();
        assert_eq!(stats.hits, vec!["logs/a"]);
        assert_eq!(stats.candidates, 1);
        assert_eq!(stats.bytes_fetched, b"abc world".len());
    }

    #[test]
    fn gzipped_docs_are_indexed_and_searched_as_text() {
        use std::io::Write;
        let gz = |data: &[u8]| {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(data).unwrap();
            enc.finish().unwrap()
        };
        let mut multi = gz(b"first line\n");
        multi.extend(gz(b"needle in second member\n"));
        let c = MemCorpus::new(
            vec![(0, "a.log.gz".into()), (1, "b.log".into())],
            vec![multi, b"plain needle\n".to_vec()],
        );
        for strategy in [Strategy::Trigram, Strategy::Sparse] {
            let (_d, r) = build_tmp(&c, strategy);
            let (matches, stats) = search_collect(&r, &c, "needle").unwrap();
            assert_eq!(
                stats.hits,
                vec!["a.log.gz", "b.log"],
                "strategy {strategy:?}"
            );
            assert_eq!(matches[0].1.line, 2);
            assert_eq!(matches[0].1.text, b"needle in second member\n".to_vec());
        }
    }

    #[test]
    fn sink_stop_ends_search_early_without_error() {
        struct StopAfterFirst;

        impl MatchSink for StopAfterFirst {
            fn on_doc(&self, _key: &str, _doc: &DocResult<'_>) -> Result<SinkFlow> {
                Ok(SinkFlow::Stop)
            }
        }

        let docs = (0..100u32).map(|i| (i, format!("doc{i}"))).collect();
        let bodies = (0..100u32)
            .map(|i| format!("needle {i}").into_bytes())
            .collect();
        let c = MemCorpus::new(docs, bodies);
        let (_d, r) = build_tmp(&c, Strategy::Trigram);
        let stats = search_streaming(
            &r,
            &c,
            "needle",
            KeyScope::default(),
            MatchOptions::default(),
            &StopAfterFirst,
        )
        .unwrap();
        // Stop is cooperative: at least one hit was reported, the search
        // ended Ok, and whatever was skipped is simply absent from hits.
        assert!(!stats.hits.is_empty());
        assert_eq!(stats.candidates, 100);
    }
}
