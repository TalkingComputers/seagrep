# holys3 OSS-readiness Plan

> Research-backed (ripgrep/tokio/clap/bat/quickwit/tantivy/object_store/arrow-rs). Two execution runs: **Run A** = docs/config/licenses/lints (mechanical), **Run B** = the trait refactor (code, gated by the differential test). CI + contributor scaffolding already landed.

## Decisions

**License:** dual `MIT OR Apache-2.0` (largest Rust cohort). Add `LICENSE-MIT` + `LICENSE-APACHE`.

**Publish scope:** CLI-first. `holys3` (cli) is publishable; the libs (`holys3-core/query/index/sigv4/s3`) get `publish = false` for now (flip later if anyone wants them as libraries). Crate names are already `holys3-*` (no `aws-sigv4` collision).

**Traits — "a trait at every seam that earns one" (rule of two: 2 impls / a test fake / an IO boundary):**

| Component                     | Action                                         | Why                                                                                                                                            |
| ----------------------------- | ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `Corpus`                      | keep (Tier 1)                                  | LocalCorpus+S3Corpus+MemCorpus fake; IO                                                                                                        |
| `BlobStore`                   | keep, minimal surface                          | Local+S3 impls; IO. Do NOT grow to mirror ObjectStore                                                                                          |
| **Index reader**              | **extract `IndexReader` trait (Run B)**        | `IndexReader`(mmap)+`StoreIndexReader`(S3) exist; `search_*` fns ~90% duplicate → collapse to `search(&dyn IndexReader, &dyn Corpus, pattern)` |
| **Credentials**               | **extract `CredentialProvider` trait (Run B)** | env+profile impls today, IMDS planned; model on AWS `ProvideCredentials` (sync)                                                                |
| Term dict                     | design shape, **defer impl**                   | FST today, sorted-table later (tantivy precedent). Don't wire trait until 2nd impl exists                                                      |
| Postings codec                | design shape, defer                            | no 2nd codec yet                                                                                                                               |
| Extractor (codec→text)        | design shape, defer                            | Stage 5 future                                                                                                                                 |
| SigV4 signer                  | **keep concrete**                              | one pure algorithm, AWS-vector-tested                                                                                                          |
| Content hasher (`hash_ngram`) | **keep concrete**                              | format-load-bearing (defines gram key + build_id); abstracting it is harmful                                                                   |
| `Strategy` enum               | **keep enum, not trait**                       | closed 2-variant set, serialized via postcard (dyn isn't Serialize)                                                                            |
| Regex verifier                | **keep concrete**                              | one matcher, pure, no 2nd planned                                                                                                              |

`IndexReader`/`CredentialProvider` must be **dyn-compatible** (sync, no generic methods, no `Self`), used as `&dyn`/`Box<dyn>`. The deferred trait shapes are documented in ARCHITECTURE.md, not yet coded.

## Run A — docs, config, licenses, lints (mechanical)

Files to add/modify:

- `LICENSE-MIT`, `LICENSE-APACHE` (standard texts; Apache header year 2026, holder "holys3 contributors").
- `[workspace.package]`: add `license = "MIT OR Apache-2.0"`, `description`, `repository`, `homepage`, `documentation`, `authors`, `keywords`, `categories` (valid slugs: `command-line-utilities`, `text-processing`, `filesystem`). Members inherit via `.workspace = true`. Libs add `publish = false`; `holys3` cli adds per-crate publish metadata (`keywords`, `categories`, `readme`).
- `[workspace.lints.rust]` + `[workspace.lints.clippy]` (clap-style set), and `[lints] workspace = true` in every member. **Fix any clippy warnings this surfaces** (doc_markdown, etc.) until `clippy --all-targets -- -D warnings` is clean.
- `rustfmt.toml` (edition/style), `clippy.toml` (`allow-unwrap-in-tests = true`, `allow-expect-in-tests = true`), `.editorconfig`.
- `README.md`: rewrite to match the **REAL CLI** (read `crates/cli/src/main.rs` — subcommands `index`/`search`/`stats`, flags `--local-dir`/`--bucket`/`--region`/`--out`/`--index`/`--strategy`/`--stats`/`--files-only`). NO invented flags (no `s3://` positional, no `--no-index`). Badges block, why/why-not, install (`cargo install holys3`), quickstart (local-dir AND bucket), how-it-works, security (hand-rolled SigV4 / private bucket), contributing+architecture pointers, dual-license footer.
- `ARCHITECTURE.md`: matklad/rust-analyzer style — Bird's Eye View, Entry Points, Code Map (one para per crate WITH an Architectural Invariant stated as a prohibition), Cross-Cutting Concerns (the differential test == ground truth; SigV4 vector conformance; the deferred trait shapes; error handling; the index-lives-in-the-bucket invariant). Use the three project invariants: index only narrows; verify is source of truth; differential test is the contract.
- `CHANGELOG.md` (keep-a-changelog, `## [Unreleased]`), `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1), `SECURITY.md` (private reporting for SigV4/auth issues).
- docs.rs metadata + `#![cfg_attr(docsrs, feature(doc_auto_cfg))]` in lib crates; crate-level `//!` docs with a runnable doctest where cheap.

Gate: `cargo test` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all -- --check` all clean; `cargo build` ok.

## Run B — trait refactor (code; differential is the gate)

1. `holys3-core`: add `pub trait IndexReader { fn docs(&self)->&[(DocId,String)]; fn strategy(&self)->Strategy; fn candidates(&self,&Query)->anyhow::Result<BTreeSet<DocId>>; fn stats(&self)->IndexStats; }` + `IndexStats{distinct_grams,terms_fst_bytes,postings_bytes}`.
2. `holys3-index`: impl `IndexReader` for both the mmap reader and `StoreIndexReader` (mmap reader wraps its infallible candidates in `Ok`). Collapse `search_matching_docs`/`search_via_store` into one `pub fn search(reader: &dyn IndexReader, corpus: &dyn Corpus, pattern: &str) -> Result<BTreeSet<DocId>>`.
3. `holys3-sigv4`: add `pub trait CredentialProvider: Send + Sync { fn provide(&self) -> anyhow::Result<Credentials>; }` with `EnvProvider`, `ProfileProvider{profile}`, and `ChainProvider(Vec<Box<dyn CredentialProvider>>)`. Re-express `resolve(profile)` as `ChainProvider(vec![EnvProvider, ProfileProvider]).provide()` — identical behavior.
4. Update CLI + tests to the trait-based `search` and provider. Document `TermDict`/`PostingsCodec`/`Extractor` shapes in ARCHITECTURE only (not coded).

Gate: BOTH differential tests (`differential`, `differential_store`) pass for both strategies; `clippy --all-targets -- -D warnings` clean; `fmt` clean; the real-S3 path unaffected (the env-gated live tests still compile).

## Self-Review

- Honors "virtual class for everything" _principled_: traits at earned seams; explicit, justified concrete list (hasher/signer/Strategy/verifier) per YAGNI + the format-load-bearing argument.
- README matches the real binary (no invented flags) — accuracy over the template.
- Differential test remains the correctness contract across the refactor.
- `docs/superpowers/` is agent scratch; consider gitignoring before a public push (kept for now as design history).
