use anyhow::Result;
use holys3_core::{testutil::MemCorpus, LocalBlobStore, Strategy};
use holys3_index::{update_index, IndexReader, SegmentedReader};
use holys3_query::Query;
use std::collections::BTreeMap;

#[test]
fn tombstone_counting_repro() -> Result<()> {
    let store_dir = tempfile::tempdir()?;
    let cache_dir = tempfile::tempdir()?;
    let store = LocalBlobStore::new(store_dir.path());

    let mut objects: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for i in 0..99 {
        objects.insert(
            format!("logs/good{i:02}"),
            format!("needle {i}").into_bytes(),
        );
    }
    let truncated_gz = {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"data").unwrap();
        enc.finish().unwrap()[..6].to_vec()
    };
    objects.insert("logs/corrupt.gz".to_owned(), truncated_gz);

    let listing: Vec<(String, String)> = objects
        .iter()
        .map(|(k, b)| (k.clone(), format!("{:016x}", holys3_core::hash_ngram(b))))
        .collect();

    let corpus_over = |keys: &[String]| {
        let docs = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (i as u32, k.clone()))
            .collect();
        let bodies = keys.iter().map(|k| objects[k].clone()).collect();
        MemCorpus::new(docs, bodies)
    };

    let report = update_index(
        &store,
        cache_dir.path(),
        Strategy::Trigram,
        &listing,
        &|keys| Ok(Box::new(corpus_over(keys))),
    )?;
    println!(
        "run1: added={} removed={} total_docs={} up_to_date={}",
        report.added, report.removed, report.total_docs, report.up_to_date
    );

    let reader = SegmentedReader::open(
        Box::new(LocalBlobStore::new(store_dir.path())),
        cache_dir.path(),
    )?;
    let candidates = reader.candidate_keys(&Query::All, None)?;
    println!(
        "reader.total_docs={} candidates(All)={}",
        reader.total_docs(),
        candidates.len()
    );

    let report2 = update_index(
        &store,
        cache_dir.path(),
        Strategy::Trigram,
        &listing,
        &|keys| Ok(Box::new(corpus_over(keys))),
    )?;
    println!(
        "run2: added={} removed={} total_docs={} up_to_date={}",
        report2.added, report2.removed, report2.total_docs, report2.up_to_date
    );

    assert_eq!(report.total_docs, 100, "writer counts tombstone as live");
    assert_eq!(reader.total_docs(), 100, "reader counts tombstone as live");
    assert_eq!(
        candidates.len(),
        99,
        "tombstone correctly excluded from candidates"
    );
    assert!(!report2.up_to_date, "tombstone forces retry every run");
    assert_eq!(report2.total_docs, 100, "still 100 after retry run");
    Ok(())
}
