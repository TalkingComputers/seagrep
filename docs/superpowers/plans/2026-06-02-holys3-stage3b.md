# holys3 Stage 3b (Index in S3) Implementation Plan

> Task-by-task. Stage 3a (on-disk FST) + the dict bake-off are committed; **trigram is the chosen S3 dict**. This stage puts the index in S3 behind a `BlobStore` abstraction, with immutable `builds/<id>/` + an atomic `CURRENT` pointer, and queries by fetching the footer + term dict once (cached) then ranged-GETting only the needed postings.

**Goal:** `holys3 index --bucket B [--prefix P]` builds the index from the bucket's own objects and writes it back to `s3://B/[P/].holys3/builds/<id>/` (+ `CURRENT`); `holys3 "<regex>" --bucket B [--prefix P]` opens the index from S3 (footer + terms.fst + manifest cached locally), resolves candidates via ranged-GET of postings, fetches candidate objects, verifies, prints. Default `--strategy trigram`.

**Architecture:** A `BlobStore` trait (`put`/`get`/`get_range`) in `holys3-core`, implemented by `LocalBlobStore` (dir-backed, for offline tests) and `S3BlobStore` (the hand-rolled client). The whole S3 protocol is written once against the trait. The SigV4 signer is generalized to any HTTP method (adds PUT). The differential test runs through the `BlobStore`-backed reader on `LocalBlobStore` (offline). A new env-gated `live_index` test exercises the real S3 round-trip.

**Tech Stack:** reuse `holys3-s3` (add PUT), `fst`, `memmap2`. Build IDs are a hex hash of the sorted `(key, etag)` manifest (deterministic; also enables "skip rebuild if unchanged").

---

## Task 1: Generalize the SigV4 signer to any method; add PUT to the client

**Files:** `crates/sigv4/src/lib.rs`, `crates/s3/src/lib.rs`

- [ ] **Step 1:** Rename the internal signer to take a method. Replace `sign_get_with_payload_hash(...)` body so it accepts `method: &str` as the first canonical-request line; keep `sign_get(...)` as a thin wrapper calling `sign_request("GET", ...)`. Add `sign_request("PUT", ...)`. Keep the AWS exact-vector test passing (it calls the GET path).

  Add to the existing tests: assert `sign_request("PUT", ...)` produces a 64-hex signature and `SignedHeaders` with the right `SignedHeaders=` list (structural check; no new AWS vector needed).

- [ ] **Step 2:** Add `S3Client::put(&self, bucket, key, body: &[u8]) -> Result<()>`: sign a PUT with `UNSIGNED-PAYLOAD`, send `body`, `error_for_status()`. Add `S3Client::get_range` if not already distinct from `get` (the Stage 1 `get` already takes `Option<(u64,u64)>` — reuse it).

- [ ] **Step 3:** `cargo test -p holys3-sigv4 -p holys3-s3`; commit `feat(sigv4,s3): generalize signer to any method + PUT`.

---

## Task 2: `BlobStore` trait + `LocalBlobStore` (core) + `S3BlobStore` (s3)

**Files:** `crates/core/src/lib.rs`, `crates/s3/src/lib.rs`

- [ ] **Step 1: Trait in core (no new deps)**

```rust
/// Append-only blob storage for index files. Names are relative paths like
/// "builds/<id>/terms.fst" or "CURRENT".
pub trait BlobStore {
    fn put(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()>;
    fn get(&self, name: &str) -> anyhow::Result<Vec<u8>>;
    fn get_range(&self, name: &str, start: u64, len: u64) -> anyhow::Result<Vec<u8>>;
}
```

- [ ] **Step 2: `LocalBlobStore` in core**

```rust
pub struct LocalBlobStore { root: std::path::PathBuf }
impl LocalBlobStore {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self { Self { root: root.into() } }
}
impl BlobStore for LocalBlobStore {
    fn put(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let p = self.root.join(name);
        if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
        std::fs::write(p, bytes)?; Ok(())
    }
    fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> { Ok(std::fs::read(self.root.join(name))?) }
    fn get_range(&self, name: &str, start: u64, len: u64) -> anyhow::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(self.root.join(name))?;
        f.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf)?; Ok(buf)
    }
}
```

Unit test: put/get/get_range round-trip on a tempdir.

- [ ] **Step 3: `S3BlobStore` in s3**

```rust
pub struct S3BlobStore { client: S3Client, bucket: String, prefix: String, rt: tokio::runtime::Handle }
// key(name) = format!("{}{}.holys3/{}", prefix, if prefix.is_empty(){""}else{"/"}, name) -- normalize once
impl holys3_core::BlobStore for S3BlobStore {
    fn put(&self, name, bytes) -> ... { block_on(self.client.put(&self.bucket, &self.key(name), bytes)) }
    fn get(&self, name) -> ...        { block_on(self.client.get(&self.bucket, &self.key(name), None)) }
    fn get_range(&self, name, start, len) -> ... {
        block_on(self.client.get(&self.bucket, &self.key(name), Some((start, start+len-1))))
    }
}
```

(Use `tokio::task::block_in_place` + `rt.block_on` as in Stage 1's `S3Corpus`.)

- [ ] **Step 4:** `cargo test -p holys3-core -p holys3-s3`; commit `feat(core,s3): BlobStore trait + Local/S3 impls`.

---

## Task 3: Build the index into a `BlobStore`; footer + CURRENT

**Files:** `crates/index/src/lib.rs`

- [ ] **Step 1: Build to a blob store under an immutable build id**

```rust
use holys3_core::{grams_index, BlobStore, Corpus, DocId, Strategy};

#[derive(serde::Serialize, serde::Deserialize)]
struct Footer { strategy: Strategy, doc_count: u32, terms_fst_len: u64, postings_len: u64, build_id: String }

/// Deterministic build id = hex of a hash over sorted (key, etag) pairs.
pub fn compute_build_id(objects: &[(String, String)]) -> String { /* sort, hash_ngram-style rapidhash over joined bytes -> hex */ }

/// Build terms.fst + postings.bin + manifest.bin + footer.bin under builds/<id>/, then write CURRENT=id.
pub fn build_to_store(corpus: &dyn Corpus, store: &dyn BlobStore, strategy: Strategy, build_id: &str) -> anyhow::Result<()> {
    // same gram->postings construction as build_to_dir, but write bytes via store.put("builds/<id>/...").
    // postings block format unchanged: [u32 count][count x u32 docid]. FST value = offset.
    // manifest.bin = postcard(Manifest { docs }). footer.bin = postcard(Footer{...}).
    // finally: store.put("CURRENT", build_id.as_bytes())
}
```

- [ ] **Step 2: Reader over a `BlobStore` (footer + cached dict + ranged postings)**

```rust
pub struct StoreIndexReader {
    map: fst::Map<memmap2::Mmap>, // terms.fst cached locally + mmap'd
    docs: Vec<(DocId, String)>,
    strategy: Strategy,
    store_postings_name: String,  // "builds/<id>/postings.bin"
    // hold a handle to the store for ranged postings reads:
}
```

`StoreIndexReader::open(store: &dyn BlobStore, cache_dir: &Path) -> Result<Self>`:

1. `id = String::from_utf8(store.get("CURRENT")?)`.
2. `footer: Footer = postcard(store.get("builds/<id>/footer.bin")?)`.
3. terms.fst: if `cache_dir/<id>/terms.fst` missing, `store.get("builds/<id>/terms.fst")` and write it to the cache; then mmap the cached file (fst::Map::new(mmap)).
4. manifest: `store.get("builds/<id>/manifest.bin")` (cache + parse) -> docs.
5. keep `store` ref + postings name for ranged reads.

`candidates(&Query)`: `Query::Gram(g)` -> `map.get(g)` -> Some(offset) -> read the block: first `get_range(postings, offset, 4)` for the count, then `get_range(postings, offset+4, count*4)` for the ids (or one ranged read if the block length is known — store len alongside the FST value if convenient; for Stage 3b two small ranged reads per gram is acceptable). And/Or/All/None as before; `All` uses the manifest doc ids.

> The reader takes `&dyn BlobStore` by storing it behind an `Arc`/reference. Simplest: make `StoreIndexReader` generic over `S: BlobStore` or hold `Box<dyn BlobStore>`. Pick `Box<dyn BlobStore>` for object safety.

- [ ] **Step 3: `search_via_store(reader, corpus, pattern)`** — same plan->candidates->verify, using `plan(pattern, reader.strategy())` and fetching candidate object bytes from `corpus` (an `S3Corpus` in prod, `MemCorpus` in tests).

- [ ] **Step 4 (GATE — offline): differential through the store reader on `LocalBlobStore`**

Add `crates/index/tests/differential_store.rs`: for each strategy, build to a `LocalBlobStore(tempdir)`, open a `StoreIndexReader` (cache in another tempdir), and assert `search_via_store == scan` for the full Stage 1 pattern list. This proves the footer+CURRENT+ranged-postings protocol is correct without S3.

- [ ] **Step 5:** `cargo test -p holys3-index`; commit `feat(index): build_to_store + StoreIndexReader (footer/CURRENT/ranged postings); offline differential green`.

---

## Task 4: CLI S3 wiring (build + search against a bucket)

**Files:** `crates/cli/src/main.rs`, `crates/cli/Cargo.toml`

- [ ] **Step 1:** Add subcommand args:
- `index --bucket B [--prefix P] [--region R] [--strategy trigram|sparse (default trigram)]`: build an `S3Corpus` (list the bucket/prefix, fetch objects), `compute_build_id`, `build_to_store(&corpus, &S3BlobStore, strategy, &id)`.
- `search "<re>" --bucket B [--prefix P] [--region R] [--files-only] [--stats]`: open `StoreIndexReader` over `S3BlobStore` (cache under `~/.cache/holys3/<bucket>/<prefix>/`), build an `S3Corpus` for verification fetches, `search_via_store`, print `key:line:col:text`.
- Keep the existing `--local-dir` build/search/stats paths working (default `--strategy trigram` there too).
- Credentials: resolve via the existing env/profile chain (`holys3_sigv4::resolve`); region from `--region` or `AWS_REGION` (no silent default — error if absent for S3 subcommands).

- [ ] **Step 2:** `cargo build -p holys3`; commit `feat(cli): index/search against an S3 bucket`.

---

## Task 5: Real-S3 end-to-end test (env-gated) + workspace green

**Files:** `crates/cli/tests/live_index.rs` (or `crates/index/tests/`), measurement note

- [ ] **Step 1: `live_index` test (gated by HOLYS3_TEST_BUCKET)**

When `HOLYS3_TEST_BUCKET` is set: build an `S3BlobStore`+`S3Corpus` against it (region from `AWS_REGION`), `build_to_store`, then `StoreIndexReader::open` + `search_via_store` for a couple of patterns and assert expected object keys are hit (the seeded corpus: `world` -> `b.txt`; `handleClick` -> `a.rs`; `EMAIL` -> `c/d.log`). Skip (print + return) when unset.

- [ ] **Step 2:** `cargo test` ; `cargo clippy --all-targets -- -D warnings` ; `cargo fmt --all -- --check`. All pass. Commit `test(cli): env-gated real-S3 index+search e2e; stage 3b green`.

---

## Self-Review (against spec §5/§15.3)

- **Spec §15.3 coverage:** in-S3 index via `BlobStore` ✓; immutable `builds/<id>/` + atomic `CURRENT` ✓; trigram dict (bake-off) ✓; footer + cached terms.fst + **ranged-GET postings** ✓; local cache ✓; statelessness = any machine reads `CURRENT`→footer→dict→ranges ✓ (offline-proven on `LocalBlobStore`, real-proven on the bucket).
- **Two gates:** offline differential through the store reader (`LocalBlobStore`) guarantees protocol correctness with no network; the env-gated `live_index` test proves the real S3 round-trip. Controller will run the live test with SSO-bridged creds against `holys3-test-381235349110-ue2`.
- **No silent defaults:** S3 subcommands require a resolvable region + creds or error; `--strategy` defaults to trigram (the decided S3 dict).
- **Deferred:** AIMD concurrency / fan-out / tail-hedging on the ranged fetches (Stage 4), incremental reindex via ETag diff + S3 Inventory (Stage: freshness), format extraction (Stage 5). Stage 3b fetches candidates sequentially — correct, not yet optimized.
