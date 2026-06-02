# holys3 Dict Bake-off (trigram vs sparse) Plan

> Decision experiment gating Stage 3b: pick the term-dict gram representation for the in-S3 dict. Adds a `--strategy` knob, parametrizes the differential gate over both, and measures `terms.fst` size + query selectivity on a realistic mid-size corpus. Stage 3a (on-disk FST) is committed and green.

**Goal:** Build the FST index with either **trigram** or **sparse** grams (byte keys), prove both pass the differential `index == scan` gate, and measure for each: `terms.fst` size, postings size, distinct grams, and per-query candidate counts (selectivity) on a mid-size normal corpus. Output a recommendation for which to use as the S3-resident dict.

**Architecture:** Add `Strategy {Trigram, Sparse}`. `build_to_dir` and `plan` take a strategy; the strategy is persisted in the manifest so `search`/reader use the same one the index was built with (mismatched strategies would break the subset invariant). Trigram "covering" == all trigrams of the literal (trigrams need no separate covering algorithm).

---

## Task 1: Trigram byte-grams + Strategy in `holys3-core`

**Files:** Modify `crates/core/src/lib.rs`

- [ ] **Step 1: Add trigram byte-grams and the Strategy enum**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strategy { Trigram, Sparse }

/// Every overlapping 3-byte window as raw bytes (sorted, deduped). <3 bytes => empty.
pub fn trigram_grams_bytes(data: &[u8]) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = data.windows(3).map(|w| w.to_vec()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Index-time grams for a strategy.
pub fn grams_index(data: &[u8], s: Strategy) -> Vec<Vec<u8>> {
    match s { Strategy::Trigram => trigram_grams_bytes(data), Strategy::Sparse => sparse_grams_all_bytes(data) }
}

/// Query-time grams for a strategy (trigram has no separate covering form).
pub fn grams_query(data: &[u8], s: Strategy) -> Vec<Vec<u8>> {
    match s { Strategy::Trigram => trigram_grams_bytes(data), Strategy::Sparse => sparse_grams_covering_bytes(data) }
}
```

Add `serde` to `crates/core/Cargo.toml` if not already present (it is, from Stage 1).

- [ ] **Step 2: Subset invariant holds trivially for trigram (covering==all); add a quick test**

```rust
#[test]
fn trigram_query_subset_of_index() {
    use std::collections::HashSet;
    let pattern = b"CONSTANT";
    let content = b"let CONSTANT = 1;";
    let all: HashSet<Vec<u8>> = grams_index(content, Strategy::Trigram).into_iter().collect();
    let q: HashSet<Vec<u8>> = grams_query(pattern, Strategy::Trigram).into_iter().collect();
    assert!(q.is_subset(&all));
}
```

- [ ] **Step 3:** `cargo test -p holys3-core`; `cargo fmt --all`; commit `feat(core): trigram byte-grams + Strategy enum`.

---

## Task 2: Thread Strategy through `holys3-query`

**Files:** Modify `crates/query/src/lib.rs`

- [ ] **Step 1: plan takes a Strategy**

```rust
use holys3_core::{grams_query, Strategy};

fn lit_query(lit: &[u8], s: Strategy) -> Query {
    let grams = grams_query(lit, s);
    if grams.is_empty() { Query::All } else { Query::And(grams.into_iter().map(Query::Gram).collect()) }
}

pub fn plan(pattern: &str, strategy: Strategy) -> anyhow::Result<Query> {
    let hir = regex_syntax::parse(pattern)?;
    let seq = regex_syntax::hir::literal::Extractor::new().extract(&hir);
    match seq.literals() {
        None => Ok(Query::All),
        Some([]) => Ok(Query::All),
        Some(lits) => {
            let branches: Vec<Query> = lits.iter().map(|l| lit_query(l.as_bytes(), strategy)).collect();
            if branches.contains(&Query::All) { Ok(Query::All) } else { Ok(Query::Or(branches)) }
        }
    }
}
```

Update the query unit tests to pass a strategy (use `Strategy::Sparse` for the existing `wildcard_is_all`/`single_char_literal_is_all`/`literal_yields_gram_conjunction`).

- [ ] **Step 2:** `cargo test -p holys3-query`; commit `feat(query): plan() takes a Strategy`.

---

## Task 3: Strategy in the index + manifest

**Files:** Modify `crates/index/src/lib.rs`

- [ ] **Step 1: Persist strategy; build uses grams_index; reader exposes it**

- Add `strategy: Strategy` to `Manifest`.
- `build_to_dir(corpus, dir, strategy)` uses `holys3_core::grams_index(&bytes, strategy)`.
- `IndexReader` stores `strategy`; add `pub fn strategy(&self) -> Strategy`.
- `search_matching_docs(reader, corpus, pattern)` calls `holys3_query::plan(pattern, reader.strategy())`.
- Update unit tests' `build_to_dir` calls to pass a strategy (use both in a loop where practical).

- [ ] **Step 2: Differential gate over BOTH strategies**

In `crates/index/tests/differential.rs`, wrap the existing assertions in a loop:

```rust
for strategy in [holys3_core::Strategy::Trigram, holys3_core::Strategy::Sparse] {
    let dir = tempfile::tempdir().unwrap();
    build_to_dir(&c, dir.path(), strategy).unwrap();
    let reader = IndexReader::open(dir.path()).unwrap();
    for p in patterns {
        let indexed = search_matching_docs(&reader, &c, p).unwrap();
        let re = regex::bytes::Regex::new(p).unwrap();
        let oracle = holys3_core::scan_matching_docs(&c, &re).unwrap();
        assert_eq!(indexed, oracle, "strategy {strategy:?} pattern `{p}`: index != scan");
    }
}
```

- [ ] **Step 3 (GATE):** `cargo test -p holys3-index --test differential -- --nocapture` — every pattern under BOTH strategies equals scan. Commit `feat(index): per-build Strategy; differential covers both`.

---

## Task 4: CLI `--strategy` + candidate stats

**Files:** Modify `crates/cli/src/main.rs`

- [ ] **Step 1:**
- `index` gains `--strategy <trigram|sparse>` (default `sparse`), passed to `build_to_dir`.
- `search` reads strategy from the opened reader (no flag), and gains `--stats` printing one line to stderr: `candidates=<N> total=<M> strategy=<S>` (N = `reader.candidates(&plan).len()`, M = `reader.docs().len()`), then prints matches as usual.
- `stats` already prints `distinct_grams`, `terms_fst_bytes`, `postings_bytes`.

- [ ] **Step 2:** smoke test both strategies on `crates`:

```bash
cargo run -p holys3 -- index --local-dir crates --out /tmp/t.idxdir --strategy trigram
cargo run -p holys3 -- index --local-dir crates --out /tmp/s.idxdir --strategy sparse
cargo run -p holys3 -- search "fn build" --local-dir crates --index /tmp/t.idxdir --stats
cargo run -p holys3 -- search "fn build" --local-dir crates --index /tmp/s.idxdir --stats
```

- [ ] **Step 3:** commit `feat(cli): --strategy + candidate --stats`.

---

## Task 5: The bake-off measurement + recommendation

**Files:** Create `docs/superpowers/notes/2026-06-02-dict-bakeoff.md`

- [ ] **Step 1: Pick a realistic mid-size NORMAL corpus**

Choose a pure-Rust crate of roughly 5–30 MB (NOT a `*-sys` crate, NOT a tiny facade). Good candidates in `~/.cargo/registry/src/*/`: `syn-2*`, `tokio-1*`, `regex-automata-0*`, or a real project checkout under `~/Desktop`/`~/Documents/Code`. Report path, byte size, file count.

- [ ] **Step 2: Build both indexes, capture sizes**

```bash
cargo run --release -p holys3 -- index --local-dir <corpus> --out /tmp/bo_tri --strategy trigram
cargo run --release -p holys3 -- index --local-dir <corpus> --out /tmp/bo_sp  --strategy sparse
cargo run --release -p holys3 -- stats --index /tmp/bo_tri
cargo run --release -p holys3 -- stats --index /tmp/bo_sp
ls -l /tmp/bo_tri /tmp/bo_sp
```

- [ ] **Step 3: Selectivity comparison**

Run ~8 representative queries (mix of short and long literals, an alternation, and a no-literal `\w+` QAll case) against BOTH indexes with `--stats`, capturing `candidates`/`total` for each. Example query set: `fn`, `struct`, `pub fn`, `impl`, `TODO`, `async fn`, `Result<`, `\w+\(`.

- [ ] **Step 4: Write the note + recommendation**

Tabulate per strategy: `distinct_grams`, `terms_fst_bytes`, `postings_bytes`, total index size, index÷corpus ratio, and the per-query candidate counts. Extrapolate `terms_fst_bytes` to a 10 GiB bucket. Then recommend the **S3-dict strategy**: favor the one whose dict is small enough to cache as a footer while keeping candidate counts low enough to be useful. State the tradeoff explicitly (sparse = fewer/longer lookups + more selective but bigger dict; trigram = smaller dict but more lookups + larger candidate sets on short patterns).

- [ ] **Step 5:** commit `docs: trigram-vs-sparse dict bake-off + S3-dict recommendation`.

---

## Task 6: Workspace green

- [ ] `cargo test` ; `cargo clippy --all-targets -- -D warnings` ; `cargo fmt --all -- --check`. Fix inline. Commit if needed.

---

## Self-Review

- **Decision coverage:** measures both axes that matter for the S3 dict — **dict size** (`terms_fst_bytes`, the Option-B cost) and **selectivity** (candidate counts, the query-cost). Differential gate now covers BOTH strategies, so neither can regress correctness.
- **Strategy persisted in manifest** → search always uses the build-time strategy (mismatch would break the subset invariant — guarded by the both-strategies differential test).
- **No silent default trap:** `--strategy` defaults to `sparse` (current behavior); trigram is opt-in for the measurement.
- **Output:** a committed note with the numbers and an explicit S3-dict recommendation that gates Stage 3b.
