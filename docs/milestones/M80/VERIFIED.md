# M80 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pyarrow-written files.
No compression ratio is claimed. The earlier fixtures came back byte-identical.

Commands: `cargo test -p pstore-format --test parquet`: 17 tests; `cargo test -p pstore-index
--test external`: 11 tests.

1. `v2_files_read_back_every_row`: `multipage-v2.parquet`, `multipage-v2-snappy.parquet` and
   `multipage-v2-zstd.parquet` (2,600 rows, several pages) locate; every extent is raw or fully
   compressed; the snappy file's pages are raw (pyarrow left them uncompressed because snappy did
   not shrink them) and the zstd file's are compressed; every row equals the generator's floats.
   Seen red first: against the parser before this change both v2 tests fail.
2. `a_v2_page_is_checked_against_its_claims`: nulls, a definition level and a repetition level are
   each refused as "levels or nulls"; a page marked uncompressed in a snappy chunk reads as raw
   (codec 0, size 0); a compressed one is sized by its on-disk bytes (codec 1, 30); a v2 header on
   a v1 page type and a v1 header on a v2 page type are refused. Seen red first, as above.
3. `an_index_over_each_codec_finds_the_exact_neighbours`: the three v2 files join the other
   codecs; each builds, reopens and searches to the exact cosine top 10 at a full window.
4. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, changed lines of `parquet.rs`): first sweep 22 mutants, 1
   missed (the `is_compressed = true` arm, redundant with the default); the arm was removed;
   re-sweep: **21 mutants, 19 caught, 2 unviable, 0 missed.** ⚠️ `cargo deny check advisories`
   still fails on `paste` (via `foyer`), as before.

**Not covered:** dictionary encoding; nullable and nested columns.
