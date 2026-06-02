use holys3_core::{scan_matching_docs, Corpus, DocId};
use holys3_index::{search_matching_docs, Index};
use std::collections::BTreeSet;

struct MemCorpus(Vec<(DocId, String)>, Vec<Vec<u8>>);
impl Corpus for MemCorpus {
    fn docs(&self) -> &[(DocId, String)] {
        &self.0
    }
    fn fetch(&self, id: DocId) -> anyhow::Result<Vec<u8>> {
        Ok(self.1[id as usize].clone())
    }
}

fn corpus() -> MemCorpus {
    let bodies: Vec<&[u8]> = vec![
        b"fn handleClick() { return 42; }",
        b"the quick brown fox",
        b"hello world\nsecond line with world",
        b"nothing interesting",
        b"EMAIL: a@b.com and c@d.org",
        b"",
        b"\xff\xfe binary-ish \x00 bytes world",
    ];
    let docs = (0..bodies.len())
        .map(|i| (i as DocId, format!("doc{i}")))
        .collect();
    MemCorpus(docs, bodies.into_iter().map(|b| b.to_vec()).collect())
}

#[test]
fn index_equals_scan_for_many_patterns() {
    let c = corpus();
    let idx = Index::build(&c).unwrap();
    let patterns = [
        "world",          // selective literal
        "handleClick",    // long literal
        "quick.*fox",     // literal + wildcard
        "EMAIL",          // uppercase literal
        r"\w+@\w+",       // no usable literal -> QAll path
        ".*",             // QAll
        "zzzznotpresent", // selective, zero matches
        "ab",             // short literal -> QAll
        "second line",    // literal with space
    ];
    for p in patterns {
        let indexed: BTreeSet<DocId> = search_matching_docs(&idx, &c, p).unwrap();
        let re = regex::bytes::Regex::new(p).unwrap();
        let oracle = scan_matching_docs(&c, &re).unwrap();
        assert_eq!(indexed, oracle, "pattern `{p}`: index != scan");
    }
}
