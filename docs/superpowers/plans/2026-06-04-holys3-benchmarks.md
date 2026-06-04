# holys3 Benchmark Suite Plan

> Research-backed (ripgrep benchsuite, tantivy/ruff benches, datafusion benchmarks crate, lance-bench `LANCE_BENCH_URI`, hyperfine, criterion, git-auto-commit + sticky-pr-comment). Two harnesses: deterministic micro (CI-gated) + end-to-end S3 macro (informational, MinIO in CI / real bucket for headline). Results embedded in README + PR comments, NO GitHub Pages.

## Decisions

- **Micro:** Criterion, `benches/`, committed small fixtures, hard-gated in CI (deterministic CPU work).
- **Macro (end-to-end S3 latency):** an `xbench` crate (`holys3-bench` binary) with `seed | upload | run | report` subcommands; backend via env (`HOLYS3_BENCH_BUCKET`+`HOLYS3_BENCH_REGION` for real S3, or `HOLYS3_BENCH_ENDPOINT` for MinIO). Reports p50/p95/p99 + prune-ratio (candidates/total) + objects-scanned/s. NOT CI-gated (network flaps); informational.
- **Reporting (no Pages):** README has `<!-- BENCH:START -->`/`<!-- BENCH:END -->` markers; CI on push to main regenerates the table and commits it back via `stefanzweifel/git-auto-commit-action@v5` (`[skip ci]`, `paths-ignore: README.md`). On PRs, `marocchino/sticky-pull-request-comment@v2` posts/updates one comment with the table + Δ vs `benches/baseline.json`.
- **No new runtime deps in shipped crates;** bench-only deps (`criterion`, the xbench crate) are isolated.

## Task 1: Criterion microbenches (`benches/` on the cli crate or a bench crate)

**Files:** `crates/index/benches/hot_paths.rs` (+ `[[bench]] harness=false` in `crates/index/Cargo.toml`), small committed fixture under `crates/index/benches/fixtures/`.

Benchmark the deterministic hot paths (no IO):

- `grams_index` + `grams_query` (trigram & sparse) over a representative text blob.
- `holys3_query::plan` for a few patterns.
- `eval_query` over an in-memory postings map (the single evaluator).
- FST build + `IndexReader` candidate lookup over a `LocalBlobStore`/mmap fixture.
- postings block decode.

Use `criterion` dev-dep. `cargo bench -p holys3-index` produces `target/criterion`. Local workflow: `--save-baseline main` / `--baseline main`.

## Task 2: `xbench` macro harness crate

**Files:** new `crates/xbench/` (`publish = false`), `crates/xbench/src/main.rs` (binary `holys3-bench`), `crates/xbench/src/{gen.rs,scenarios.rs}`, `crates/xbench/scenarios/queries.toml`.

- `seed --seed <u64> --objects <N> --size <bytes>`: deterministic synthetic corpus (fixed RNG) with a KNOWN per-query match rate, written to a temp dir. Each scenario's expected hit count is recorded so `run` asserts correctness (not silently returning nothing — ripgrep's match-count check).
- `upload --target <dir|s3>`: upload the seeded corpus to the env backend (real S3 via the holys3-s3 client, or MinIO via `HOLYS3_BENCH_ENDPOINT`).
- `run --scenarios queries.toml --iterations <N> --warmup <N> [--concurrency N]`: for each scenario, time end-to-end `search` over the backend; record p50/p95/p99, candidates/total (prune ratio), bytes fetched, and assert the expected hit count. Also run an indexed-vs-`--concurrency 1` comparison to show fan-out speedup.
- `report --out runs/<id>.json` + `compare <a.json> <b.json>`: emit a summary JSON (committed to `crates/xbench/runs/`) and a Markdown table; `compare` prints p50/p95 deltas (datafusion pattern).
- Scenario set: short literal, long literal, alternation, anchored, no-match, QAll (`.*`).

## Task 3: MinIO for repeatable local/CI runs

**Files:** `docker-compose.bench.yml` (MinIO + a one-shot `mc` bucket-create), `crates/xbench/README.md` (how to run).

`HOLYS3_BENCH_ENDPOINT=http://localhost:9000` path-style. Requires the s3 client to support a custom endpoint + path-style addressing (see Task 5 — small addition to `S3Client`, gated by a config field; do NOT change AWS default behavior).

## Task 4: CI bench workflow (no Pages)

**Files:** `.github/workflows/bench.yml`, `scripts/render-bench-table.sh` (or a `holys3-bench render` subcommand), README markers.

- `micro` job (PR + main): `cargo bench -p holys3-index -- --save-baseline pr`; compare to a committed `benches/baseline.json`; **hard-fail** on >X% regression (start 10%). Post results into the sticky PR comment.
- `e2e` job (PR + main): start MinIO service, `holys3-bench seed|upload|run` against it, produce the markdown table — **informational only** (no fail-on-regression; network).
- On **push to main**: regenerate the README `<!-- BENCH -->` section from the latest run and commit via `stefanzweifel/git-auto-commit-action@v5` with `[skip ci]`; update `benches/baseline.json`.
- On **PR**: `marocchino/sticky-pull-request-comment@v2` with the micro deltas + e2e table.

## Task 5: S3 custom endpoint (MinIO support) — minimal

**Files:** `crates/s3/src/lib.rs`.

Add an optional `endpoint: Option<String>` + `path_style: bool` to `S3Client`/`S3BlobStore`/`S3Corpus` config; when set, address as `{endpoint}/{bucket}/{key}` (path-style) instead of the virtual-host `{bucket}.s3.{region}.amazonaws.com`. SigV4 host header follows the endpoint. AWS default path unchanged (no endpoint = today's behavior). This is the only shipped-code change; keep it surgical and covered by the existing live test shape.

## Task 6: README results section + gates

- Add the `<!-- BENCH:START/END -->` block with a placeholder table + the methodology note (instance/region, object count/size, holys3 SHA, the exact command, "one machine, reproduce with `make bench`").
- `make bench-micro` (criterion) / `make bench-s3` (xbench against your bucket) / `make bench-minio` (xbench against docker MinIO).
- Gate: `cargo test`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` all clean; `cargo bench -p holys3-index --no-run` compiles; live/bench tests gated by env so CI without creds passes.

## Self-Review

- Micro = deterministic + gated; macro = informational (no flaky network gate) — matches what every speed-focused project actually does.
- Numbers live in the README (auto-committed) + PR comments; no Pages.
- Correctness preserved: xbench asserts expected hit counts; the differential tests are untouched and stay green. Only shipped-code change is the optional S3 endpoint (Task 5), behind config, AWS default unchanged.
- No backwards-compat shims (project standard); xbench + criterion are `publish=false`/dev-only.
