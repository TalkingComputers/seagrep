//! Streaming search engine: concurrent fetch, parallel decompress+verify,
//! per-doc result sinks.

use crate::{IndexReader, SearchStats};
use anyhow::Result;
use holys3_core::{decode_body, matches_in, Corpus, DocId, Match};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Whether to keep streaming results after a sink call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkFlow {
    Continue,
    /// End the search early and report success (e.g. downstream pipe closed).
    Stop,
}

/// Receives verified results per doc, possibly from several threads at once.
pub trait MatchSink: Sync {
    /// Whether this sink uses match positions. Returning false lets the
    /// engine stop at the first match per doc (files-only behavior); `on_doc`
    /// is then called with empty `matches`.
    fn wants_matches(&self) -> bool {
        true
    }

    /// Called once per doc with at least one verified match.
    fn on_doc(&self, key: &str, matches: &[Match]) -> Result<SinkFlow>;
}

/// Sentinel threaded through `fetch_each` to unwind an early stop.
#[derive(Debug)]
struct StopEarly;

impl std::fmt::Display for StopEarly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("search stopped early by sink")
    }
}

impl std::error::Error for StopEarly {}

fn lock<'a, T>(mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>> {
    mutex
        .lock()
        .map_err(|_| anyhow::anyhow!("a search worker panicked"))
}

/// Streaming search: candidate docs are fetched concurrently, decompressed
/// and regex-verified on a worker pool, and reported to `sink` per doc as
/// they complete (unordered across docs; in-order within a doc). Memory is
/// bounded by fetch concurrency + worker count, not corpus size.
///
/// `key_filter` prunes candidates by object key before anything is fetched.
/// When the sink does not want match positions, verification stops at the
/// first match per doc.
pub fn search_streaming(
    reader: &dyn IndexReader,
    corpus: &dyn Corpus,
    pattern: &str,
    key_filter: Option<&dyn Fn(&str) -> bool>,
    sink: &dyn MatchSink,
) -> Result<SearchStats> {
    let q = holys3_query::plan(pattern, reader.strategy())?;
    let mut ids = reader.candidates(&q)?;
    if let Some(filter) = key_filter {
        let docs = reader.docs();
        ids.retain(|&id| filter(&docs[id as usize].1));
    }
    let candidates = ids.len();
    let re = regex::bytes::Regex::new(pattern)?;
    if ids.is_empty() {
        return Ok(SearchStats {
            hits: Vec::new(),
            candidates,
            total_docs: reader.docs().len(),
            bytes_fetched: 0,
        });
    }
    let doc_keys = corpus.docs();
    let workers = std::thread::available_parallelism()?.get().min(ids.len());

    let bytes_fetched = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let hits: Mutex<Vec<DocId>> = Mutex::new(Vec::new());
    let worker_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let (tx, rx) = std::sync::mpsc::sync_channel::<(DocId, Vec<u8>)>(workers * 2);
    let rx = Mutex::new(rx);

    // Workers never exit while the channel is open: on error they record it,
    // flip `stopped`, and keep draining, so the feeder's blocking send always
    // has a consumer (otherwise an all-workers-failed state would deadlock
    // the feeder).
    let record_error = |err: anyhow::Error| {
        if let Ok(mut slot) = worker_error.lock() {
            slot.get_or_insert(err);
        }
        stopped.store(true, Ordering::Relaxed);
    };
    let wants_matches = sink.wants_matches();
    let verify = |id: DocId, bytes: Vec<u8>| -> Result<()> {
        let key = &doc_keys[id as usize].1;
        let text = match decode_body(key, bytes) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("warning: {err:#}; skipping");
                return Ok(());
            }
        };
        let matches = if wants_matches {
            let matches = matches_in(id, &text, &re);
            if matches.is_empty() {
                return Ok(());
            }
            matches
        } else {
            if !re.is_match(&text) {
                return Ok(());
            }
            Vec::new()
        };
        lock(&hits)?.push(id);
        if sink.on_doc(key, &matches)? == SinkFlow::Stop {
            stopped.store(true, Ordering::Relaxed);
        }
        Ok(())
    };

    let feed_result = std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let received = match lock(&rx) {
                    Ok(guard) => guard.recv(),
                    Err(err) => {
                        record_error(err);
                        return;
                    }
                };
                let Ok((id, bytes)) = received else {
                    return;
                };
                if stopped.load(Ordering::Relaxed) {
                    continue;
                }
                // catch_unwind keeps a panicking verify (or sink) from
                // breaking the drain invariant — and from poisoning the rx
                // mutex, since the panic never crosses the recv lock.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| verify(id, bytes))) {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => record_error(err),
                    Err(_) => record_error(anyhow::anyhow!("a search worker panicked")),
                }
            });
        }
        let feed = corpus.fetch_each(&ids, &mut |id, bytes| {
            if stopped.load(Ordering::Relaxed) {
                return Err(anyhow::Error::new(StopEarly));
            }
            bytes_fetched.fetch_add(bytes.len(), Ordering::Relaxed);
            tx.send((id, bytes))
                .map_err(|_| anyhow::anyhow!("search workers exited early"))
        });
        drop(tx);
        feed
    });

    if let Some(err) = lock(&worker_error)?.take() {
        return Err(err);
    }
    match feed_result {
        Err(err) if err.is::<StopEarly>() => {}
        other => other?,
    }

    let mut hits = hits
        .into_inner()
        .map_err(|_| anyhow::anyhow!("a search worker panicked"))?;
    hits.sort_unstable();
    Ok(SearchStats {
        hits,
        candidates,
        total_docs: reader.docs().len(),
        bytes_fetched: bytes_fetched.into_inner(),
    })
}

/// Discards results; pairs with `SearchStats.hits` when only hit docs
/// matter. `wants_matches` is false, so the engine early-exits per doc.
pub struct NullSink;

impl MatchSink for NullSink {
    fn wants_matches(&self) -> bool {
        false
    }

    fn on_doc(&self, _key: &str, _matches: &[Match]) -> Result<SinkFlow> {
        Ok(SinkFlow::Continue)
    }
}

#[derive(Default)]
struct CollectSink {
    matches: Mutex<Vec<Match>>,
}

impl MatchSink for CollectSink {
    fn on_doc(&self, _key: &str, matches: &[Match]) -> Result<SinkFlow> {
        lock(&self.matches)?.extend_from_slice(matches);
        Ok(SinkFlow::Continue)
    }
}

/// Convenience for tests and benchmarks: collect every match, globally
/// sorted by (doc, line, col, text).
pub fn search_collect(
    reader: &dyn IndexReader,
    corpus: &dyn Corpus,
    pattern: &str,
) -> Result<(Vec<Match>, SearchStats)> {
    let sink = CollectSink::default();
    let stats = search_streaming(reader, corpus, pattern, None, &sink)?;
    let mut matches = sink
        .matches
        .into_inner()
        .map_err(|_| anyhow::anyhow!("a search worker panicked"))?;
    matches.sort_by(|a, b| {
        (a.doc, a.line, a.col, a.text.as_str()).cmp(&(b.doc, b.line, b.col, b.text.as_str()))
    });
    Ok((matches, stats))
}
