# holys3 Stage 4 (Concurrency) Implementation Plan

> Make S3 I/O massively concurrent without breaking the ARCHITECTURE invariants. Research-backed (object_store, aws-c-s3, Netflix concurrency-limits, AWS full-jitter, rup12.net 10k-files). Correctness gate = the two differential tests stay green.

**Goal:** fix the current N-serial-`block_on` bug; fetch candidates (search) and objects (build) with **bounded concurrent ranged GETs**: AIMD adaptive concurrency, full-jitter backoff on 503 SlowDown with a retry budget, and tail hedging — all inside `holys3-s3`; CPU verify on rayon.

**Key design (from research):** keep `Corpus`/`BlobStore` **sync + dyn-safe** (do NOT make them async-fn-in-trait — that isn't dyn-compatible and would async-color the pure crates). Add ONE batched method `Corpus::fetch_many` with a default sequential impl; the S3 impl does the whole fan-out under a **single** `block_on`. `s3` owns the concurrency _mechanism_; `cli` owns the _policy_ (the `FetchConfig` knobs). `index` stays pure (no reqwest, no runtime) — it calls `fetch_many` then verifies on rayon.

---

## Task 1: `Corpus::fetch_many` (core)

**Files:** `crates/core/src/lib.rs`

- [ ] Add to the `Corpus` trait (dyn-safe, sync, default impl so Local/Mem corpora need no change):

```rust
/// Fetch many docs concurrently. Result order is NOT guaranteed; each item carries its DocId.
/// Per-item Result so one bad object doesn't abort a large batch. Default = sequential.
fn fetch_many(&self, ids: &[DocId]) -> anyhow::Result<Vec<(DocId, anyhow::Result<Vec<u8>>)>> {
    Ok(ids.iter().map(|&id| (id, self.fetch(id))).collect())
}
```

`BlobStore` unchanged (postings reads are small/serial). `cargo test -p holys3-core`; commit `feat(core): Corpus::fetch_many batched fetch (default sequential)`.

---

## Task 2: Concurrent fetch machinery (s3)

**Files:** `crates/s3/Cargo.toml` (add `rand`, `futures`), new `crates/s3/src/fetch.rs`, `crates/s3/src/lib.rs`

- [ ] **FetchConfig** (knobs; constructed by cli, defaults here):

```rust
#[derive(Clone)]
pub struct FetchConfig {
    pub start: usize,      // 64  initial AIMD concurrency
    pub cap: usize,        // 750 hard ceiling
    pub buffer: usize,     // 1000 buffer_unordered depth (> cap)
    pub max_retries: u32,  // 5
    pub backoff_base_ms: u64, // 50
    pub backoff_cap_ms: u64,  // 20_000
    pub hedge_after: std::time::Duration, // 2s (AWS <512KB guidance)
    pub retry_tokens: usize,  // 500 global retry/hedge budget
}
impl Default for FetchConfig { /* the values above */ }
```

- [ ] **AimdLimiter** (tokio `Semaphore` + atomic limit; additive-increase on success, multiplicative-decrease (halve) on 503, via `add_permits` / `try_acquire_many(..).forget()`), **RetryBudget** (atomic token bucket: `try_take`/`refund`), **get_with_retry** (full jitter: `delay = rand(0..=min(cap, base*2^attempt))`, retry only on 503, take a budget token, retry on a fresh connection), **fetch_one_hedged** (`tokio::select!{ biased; primary; _ = sleep(hedge_after) => race a second GET if budget allows }`). Use the exact shapes from the research (agent report 3).

- [ ] **Override `S3Corpus::fetch_many`**: build the keys, then under ONE `block_in_place(|| rt.block_on(async { ... }))`:

```rust
let limiter = Arc::new(AimdLimiter::new(cfg.start, cfg.cap));
let budget = Arc::new(RetryBudget::new(cfg.retry_tokens));
let results = futures::stream::iter(keys.into_iter().map(|(id, key)| {
    let (client, bucket, limiter, budget, cfg) = (..clones..);
    async move { (id, fetch_one_hedged(&client, &bucket, &key, &limiter, &budget, &cfg).await) }
}))
.buffer_unordered(cfg.buffer).collect::<Vec<_>>().await;
Ok(results)
```

- [ ] **Tune the shared `reqwest::Client`** (one Client, cloned everywhere): `pool_max_idle_per_host(cfg.cap)`, `pool_idle_timeout(60s)`, `tcp_keepalive(30s)`, `http2_adaptive_window(true)`, `connect_timeout(3s)`, `timeout(20s)`. Keep a second `pool_max_idle_per_host(0)` client for retries (fresh connection). Do NOT use `http2_prior_knowledge`.

- [ ] `cargo test -p holys3-s3` (parser + live compile); `cargo clippy -p holys3-s3 --all-targets -- -D warnings`; commit `feat(s3): concurrent fetch_many (AIMD + full-jitter retry + tail hedge)`.

---

## Task 3: Use concurrency in search + build, verify on rayon (index)

**Files:** `crates/index/Cargo.toml` (add `rayon`), `crates/index/src/lib.rs`

- [ ] `search` calls `fetch_many(&candidate_ids)` once, then verifies in parallel on rayon (regex off the reactor):

```rust
let ids: Vec<DocId> = reader.candidates(&q)?.into_iter().collect();
let fetched = corpus.fetch_many(&ids)?;
let hits = fetched.par_iter().filter_map(|(id, b)| match b {
    Ok(bytes) if re.is_match(bytes) => Some(*id), _ => None,
}).collect();
```

- [ ] `build_to_store`/`build_to_dir`: replace the sequential `for id { corpus.fetch(id) }` with one `fetch_many(all_ids)` then gram-extract on rayon (`par_iter`), merging postings (collect per-doc gram lists, then build the BTreeMap). Keep output identical (sort/dedup) so the index bytes are unchanged.
- [ ] `index` stays pure: no reqwest, no tokio runtime — it only calls the trait + rayon.

- [ ] **GATE:** `cargo test -p holys3-index --test differential` AND `--test differential_store` (both strategies) MUST still pass (MemCorpus uses the sequential default; correctness unchanged). `cargo clippy -p holys3-index --all-targets -- -D warnings`. Commit `feat(index): concurrent fetch + rayon verify/extract`.

---

## Task 4: CLI wires FetchConfig (policy)

**Files:** `crates/cli/src/main.rs`

- [ ] Add `--concurrency <N>` (maps to `FetchConfig.cap`, default 750) and optionally `--no-hedge`. Build a `FetchConfig` and pass it into `S3Corpus::new`/`S3BlobStore::new`. Behavior identical for `--local-dir` (sequential default).
- [ ] `cargo build -p holys3`; commit `feat(cli): --concurrency flag wires FetchConfig`.

---

## Task 5: Workspace green

- [ ] `cargo test` ; `cargo clippy --workspace --all-targets -- -D warnings` ; `cargo fmt --all -- --check`. Live tests (live_s3, live_index) compile. Commit if needed.

---

## Self-Review

- Invariants preserved: `s3` = sole network IO (all reqwest fan-out there); `cli` = concurrency policy (FetchConfig); `core`/`query`/`index`/`sigv4` stay sync+pure (index uses rayon, not tokio).
- The only trait signature change is the default-impl `fetch_many` (dyn-safe, no async).
- Correctness: differential tests unchanged + still green (sequential default for fakes); real-S3 concurrency verified by the controller via the live path after re-auth.
- Defaults committed (start 64 / cap 750 / buffer 1000 / 8MiB ranges / 50ms–20s backoff / 2s hedge / 500 tokens) per AWS + rup12.net evidence; tunable via --concurrency.
