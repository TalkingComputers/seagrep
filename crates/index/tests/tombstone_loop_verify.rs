//! TEMPORARY verification test for review finding: permanently undecodable
//! object causes update_index to report +1/-1 forever, never up_to_date.

use anyhow::Result;
use holys3_core::{testutil::MemCorpus, LocalBlobStore, Strategy};
use holys3_index::update_index;
use std::collections::BTreeMap;

fn listing(objects: &BTreeMap<String, Vec<u8>>) -> Vec<(String, String)> {
    objects
        .iter()
        .map(|(key, body)| {
            (
                key.clone(),
                format!("{:016x}", holys3_core::hash_ngram(body)),
            )
        })
        .collect()
}

#[test]
fn permanently_undecodable_object_never_converges() -> Result<()> {
    let store_dir = tempfile::tempdir()?;
    let cache_dir = tempfile::tempdir()?;
    let store = LocalBlobStore::new(store_dir.path());

    let mut objects: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    objects.insert("a.txt".into(), b"hello world".to_vec());
    let mut corrupt = vec![0x1f, 0x8b];
    corrupt.extend_from_slice(b"\xff\xff this is not a gzip stream at all");
    objects.insert("bad.gz".into(), corrupt);

    let listing = listing(&objects);
    let make_corpus = |keys: &[String]| -> Result<Box<dyn holys3_core::Corpus>> {
        let docs = keys
            .iter()
            .enumerate()
            .map(|(i, key)| (i as u32, key.clone()))
            .collect();
        let bodies = keys.iter().map(|key| objects[key].clone()).collect();
        Ok(Box::new(MemCorpus::new(docs, bodies)) as Box<dyn holys3_core::Corpus>)
    };

    for run in 1..=4 {
        let report = update_index(
            &store,
            cache_dir.path(),
            Strategy::Trigram,
            &listing,
            &make_corpus,
        )?;
        eprintln!(
            "run {run}: added={} removed={} total={} segments={} up_to_date={}",
            report.added, report.removed, report.total_docs, report.segments, report.up_to_date
        );
        if run == 1 {
            assert!(!report.up_to_date);
            assert_eq!(report.added, 2);
        } else {
            assert!(
                !report.up_to_date,
                "run {run} unexpectedly converged — finding would be FALSE"
            );
            assert_eq!(report.added, 1, "run {run}");
            assert_eq!(report.removed, 1, "run {run}");
        }
    }
    Ok(())
}
