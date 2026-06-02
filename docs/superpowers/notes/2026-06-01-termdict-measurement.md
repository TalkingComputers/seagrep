# Term-dict measurement: spec section 5 A/B

## Corpus

- Path: `/Users/parsabahraminejad/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/aws-lc-sys-0.41.0`
- `du -sh`: `67M`
- `du -sk`: `68264` KiB = `69902336` bytes
- File count indexed: `1959`

## Commands

```bash
cargo run --release -p holys3 -- index --local-dir /Users/parsabahraminejad/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/aws-lc-sys-0.41.0 --out /tmp/big.idx
cargo run --release -p holys3 -- stats --index /tmp/big.idx
ls -l /tmp/big.idx
```

## Measured output

```text
indexed 1959 docs -> /tmp/big.idx
distinct_trigrams=243823
termdict_bytes_estimate=3901168
total_postings=3894032
-rw-r--r--@ 1 parsabahraminejad  wheel  9055150 Jun  2 03:13 /tmp/big.idx
```

- Distinct trigrams: `243823`
- Term-dict estimate: `3901168` bytes = `3.72` MiB
- Total postings: `3894032`
- On-disk index size: `9055150` bytes = `8.64` MiB

## Sparse n-gram extrapolation

Stage 1 term-dict entry size is `3901168 / 243823 = 16` bytes per gram.

Sparse n-grams at about 2x trigram count:

```text
sparse_distinct_grams = 243823 * 2 = 487646
sparse_termdict_bytes = 487646 * 16 = 7802336 bytes = 7.44 MiB
```

## Multi-GB target extrapolation

Use a 10 GiB bucket as the representative multi-GB target.

```text
target_bytes = 10 * 1024^3 = 10737418240
scale = 10737418240 / 69902336 = 153.606
trigram_target_termdict = 3901168 * 153.606 = 599242813 bytes = 571.48 MiB
sparse_target_termdict = 7802336 * 153.606 = 1198485625 bytes = 1142.96 MiB
```

For reference, a 2 GiB bucket would extrapolate to about `239697125` bytes = `228.59` MiB for the sparse term dict.

## Recommendation

Choose Option B: FST blueprint with the dict in S3.

The 10 GiB target extrapolates to about `1.20` GB (`1142.96` MiB) for the sparse n-gram term dict, which is not well under a few hundred MB. Option A is only acceptable for buckets near the 2 GiB reference point or smaller, where the sparse term dict stays around `229` MiB.
