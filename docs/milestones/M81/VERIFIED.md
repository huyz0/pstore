# M81 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pyarrow-written files.
No compression ratio is claimed. Earlier fixtures came back byte-identical.

Commands: `cargo test -p pstore-format --test parquet`: 22 tests; `cargo test -p pstore-format
--lib`: 19 tests; `cargo test -p pstore-index --test external`: 11 tests.

1. `dictionary_files_read_back_every_row`: `dictionary.parquet`, `dictionary-snappy.parquet` and
   `dictionary-v2.parquet` (2,600 rows over 40 distinct vectors, several index pages) locate with
   one shared dictionary of 40 and every row equals the generator's floats; the snappy file's
   pages and dictionary are codec 1. `dict.parquet` (a dictionary per row group) reads back too.
2. `rle_and_bit_packed_runs_decode_and_mix` and
   `a_body_that_is_not_n_indices_below_count_is_refused`: RLE, bit-packed (3 and 17 bits), mixed
   runs, a run cut by `n`, a six-byte run header, 2^28 repeats, widths 0 and 32 decode; widths 33
   and 40, a seven-byte header, zero-length runs, an index equal to `count`, too few indices and
   runs cut short are refused.
3. `a_dictionary_is_read_once_for_all_its_candidates`: two rows of one page move that page and the
   dictionary once; rows on two pages move both pages and still one dictionary.
4. `a_damaged_dictionary_page_is_an_error`: a scribbled index page, a dictionary claiming one
   vector too many, and extents with an empty, past-the-object, overflowing or body-less
   dictionary are each an error; one ending exactly at the object's end is fine.
5. `a_dictionary_chunk_is_checked_page_by_page`, `an_index_page_is_checked_against_its_sizes` and
   `locate_refuses_each_shape_it_cannot_read`: dictionary and index page sizes, encodings, the
   256 MiB bound at and one past its edge, a dictionary offset of 0 meaning none, a dictionary out
   of place or doubled, and indices before any dictionary are each refused by name or read.
6. `an_index_over_each_codec_finds_the_exact_neighbours`: the three dictionary files join the
   other codecs: each builds, reopens and searches to the exact cosine top 10 at a full window.
7. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, changed lines): format side 157 mutants, 153 caught, 3
   unviable, 1 missed (`parquet.rs:601`, `||`→`&&`) which a new test kills (re-run of that
   mutant family: 14 of 14 caught); index side 6 mutants, 5 caught, 1 unviable, 0 missed. Earlier
   sweeps of this milestone found boundary misses in the index decoder, the extent validator and
   the page locator; each is killed by the tests named above. ⚠️ `cargo deny check advisories` still
   fails on `paste` (via `foyer`), as before.

**Not covered:** page checksums are not verified (a scribbled dictionary changes floats
undetectably); nullable and nested columns; a dictionary larger than 256 MiB. A `PXI4` blob is
read as not an external index and must be rebuilt; none exists outside this branch.
