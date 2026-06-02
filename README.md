# holys3

ripgrep over a private S3 bucket, accelerated by a trigram index (Stage 1).
See `docs/superpowers/specs/2026-06-01-holys3-design.md` for the full design.

## Try it (local corpus)

    cargo run -p holys3 -- index  --local-dir <dir> --out idx
    cargo run -p holys3 -- search "<regex>" --local-dir <dir> --index idx
    cargo run -p holys3 -- stats  --index idx
