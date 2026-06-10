use anyhow::Result;
use holys3_core::{testutil::MemCorpus, LocalBlobStore, Strategy};
use holys3_index::{update_index, IndexReader, SegmentedReader};

#[test]
fn corrupt_postings_values_error_not_panic() -> Result<()> {
    let store_dir = tempfile::tempdir()?;
    let cache_dir = tempfile::tempdir()?;
    let store = LocalBlobStore::new(store_dir.path());
    let listing = vec![
        ("a".to_owned(), "etag-a".to_owned()),
        ("b".to_owned(), "etag-b".to_owned()),
    ];
    update_index(
        &store,
        cache_dir.path(),
        Strategy::Trigram,
        &listing,
        &|keys| {
            let docs = keys
                .iter()
                .enumerate()
                .map(|(i, k)| (i as u32, k.clone()))
                .collect();
            let bodies = keys.iter().map(|_| b"hello world".to_vec()).collect();
            Ok(Box::new(MemCorpus::new(docs, bodies)))
        },
    )?;

    let mut postings_path = None;
    for entry in std::fs::read_dir(store_dir.path().join("segments"))? {
        let p = entry?.path().join("postings.bin");
        if p.exists() {
            postings_path = Some(p);
        }
    }
    let postings_path = postings_path.expect("segment postings.bin on disk");
    let len = std::fs::metadata(&postings_path)?.len() as usize;
    std::fs::write(&postings_path, vec![0xffu8; len])?;

    let reader = SegmentedReader::open(
        Box::new(LocalBlobStore::new(store_dir.path())),
        cache_dir.path(),
    )?;
    let q = holys3_query::plan("hello", Strategy::Trigram)?;
    let result = reader.candidate_keys(&q, None);
    assert!(result.is_err(), "expected Err, got {result:?}");
    Ok(())
}
