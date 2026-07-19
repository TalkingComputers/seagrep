//! Index discovery for searches whose default index location is empty. An
//! index built at `s3://bucket/logs` serves a search of
//! `s3://bucket/logs/2026/07`: the reader validates that the index covers
//! the requested source and the search scopes itself to the narrower
//! prefix. So when `<prefix>/.seagrep` has no index, the parent prefixes
//! are probed up to the bucket root. Explicit `--index` locations are
//! remembered per source in a local cache file and used as the last
//! fallback, so repeated searches of a read-only source need no flags.
//!
//! Discovery runs only after the default location reports `IndexMissing`;
//! the happy path pays nothing.

use crate::{
    build_cache_dir, build_source_identity, open_index_storage, IndexArgs, IndexStorage, S3Source,
};
use anyhow::Result;
use seagrep_index::SegmentedReader;
use seagrep_s3::build_index_namespace;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
struct RememberedIndex {
    location: String,
    index_region: Option<String>,
    index_endpoint: Option<String>,
}

fn map_path() -> Result<PathBuf> {
    let mut path = seagrep_core::cache_home()?;
    path.push("seagrep");
    path.push("index-locations.json");
    Ok(path)
}

fn read_map() -> BTreeMap<String, RememberedIndex> {
    map_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn source_key(source: &S3Source) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        source.endpoint, source.bucket, source.prefix
    )
}

/// Record an explicit `--index` location for this source. Best-effort: the
/// map is a convenience cache, never load-bearing, so failures are ignored.
pub(crate) fn remember_index(source: &S3Source, index: &IndexArgs) {
    let Some(location) = index.location.clone() else {
        return;
    };
    let entry = RememberedIndex {
        location,
        index_region: index.index_region.clone(),
        index_endpoint: index.index_endpoint.clone(),
    };
    let mut map = read_map();
    if map.get(&source_key(source)) == Some(&entry) {
        return;
    }
    map.insert(source_key(source), entry);
    let Ok(path) = map_path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let staged = path.with_extension("json.tmp");
    let Ok(bytes) = serde_json::to_vec_pretty(&map) else {
        return;
    };
    if std::fs::write(&staged, bytes).is_err() {
        return;
    }
    let _ = std::fs::rename(&staged, &path);
}

/// Parent prefixes of the searched prefix, nearest first, ending at the
/// bucket root. The searched prefix itself is excluded — its default
/// namespace already failed before discovery runs.
fn parent_chain(prefix: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = prefix.trim_matches('/');
    while let Some((parent, _)) = current.rsplit_once('/') {
        chain.push(parent.to_owned());
        current = parent;
    }
    if !prefix.trim_matches('/').is_empty() {
        chain.push(String::new());
    }
    chain
}

fn storage_at(source: &S3Source, prefix: &str) -> Result<IndexStorage> {
    let root = build_index_namespace(prefix).trim_matches('/').to_owned();
    let endpoint = source.client.endpoint_identity();
    let cache = build_cache_dir(Some(&endpoint), &source.bucket, &root)?;
    Ok(IndexStorage {
        client: source.client.clone(),
        endpoint,
        bucket: source.bucket.clone(),
        root,
        cache,
    })
}

/// Find an index for a source whose default location is empty: parent
/// prefixes first, then the remembered location from an earlier `--index`
/// run. `None` means nothing usable was found anywhere.
pub(crate) fn discover_fallback(
    source: &S3Source,
    concurrency: usize,
) -> Result<Option<IndexStorage>> {
    let identity = build_source_identity(source);
    for candidate in parent_chain(&source.prefix) {
        let storage = storage_at(source, &candidate)?;
        let present = match storage.store().get_versioned("segments.bin") {
            Ok(present) => present.is_some(),
            // A parent the caller cannot read must not fail the search.
            Err(error) => {
                eprintln!(
                    "note: cannot probe index at {}: {error:#}",
                    storage.location()
                );
                false
            }
        };
        if !present {
            continue;
        }
        // The full open validates the format and that this index covers the
        // requested source; a parent index built for a sibling is skipped.
        match SegmentedReader::open(storage.store(), storage.cache(), &identity) {
            Ok(_) => {
                eprintln!(
                    "note: using index at {} (discovered at a parent prefix; pass --index to override)",
                    storage.location()
                );
                return Ok(Some(storage));
            }
            Err(error) => {
                eprintln!("note: skipping index at {}: {error:#}", storage.location());
            }
        }
    }
    if let Some(entry) = read_map().remove(&source_key(source)) {
        let args = IndexArgs {
            location: Some(entry.location),
            index_region: entry.index_region,
            index_endpoint: entry.index_endpoint,
        };
        let storage = open_index_storage(source, &args, concurrency)?;
        eprintln!(
            "note: using remembered index {} (recorded from an earlier --index run)",
            storage.location()
        );
        return Ok(Some(storage));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_chain_walks_to_root() {
        assert_eq!(parent_chain("raw/rcaeval"), vec!["raw", ""]);
        assert_eq!(parent_chain("a/b/c"), vec!["a/b", "a", ""]);
        assert_eq!(parent_chain("logs"), vec![""]);
        assert!(parent_chain("").is_empty());
        assert_eq!(parent_chain("/a/b/"), vec!["a", ""]);
    }
}
