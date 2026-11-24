# M289 verification

The spec was reviewed by its author and amended while the tests were written (recorded
there). Code review is recorded in the commit body.

How to run the tests: `cargo test -p pstore-format -- narrow`,
`cargo test -p pstore-engine --lib -- head::` and
`cargo test -p pstore-server --test it -- vector_storage:: storage::`.

Red run: the new tests call what the parent does not have (`pstore_format::narrow`,
`BlockOptions::vectors`, `Storage::vectors`) and do not compile on it, so none was observed
failing there. Instead each mutation below was applied one at a time by a script, the tests
of its crate run, and the source restored: **13 of 13 killed**. No `cargo mutants` run is
claimed; CI's incremental mutants job on the PR is the gate.

- i8 ties rounded away from even; the canonical exponent loop removed; the exponent
  stepped down two at a time; the exponent check at `i8::MAX` instead of `i8::MIN`; f16's
  overflow fallback removed (`narrow.rs`).
- The writer's tail taken as 0; version 4 written for any columnar segment with a dense
  field; narrow written under row blocks (`writer.rs`).
- `field_rows` slicing by `dims * 4`; the legacy `Vectors` readers decoding f32 always
  (`reader.rs`).
- The engine passing no tail for euclidean (`lib.rs`); an override ignored at seal; the
  f16 and i8 HEAD codes swapped (`head.rs`).

## Criteria

1. Codec: `narrow::tests::*` (7 tests: lengths, kind and name codes, i8 integers exact and
   the smallest exponent, ties to even and the half-step bound at scales 1e-30 to 1e30 with
   re-encoding byte-equal, f16 ties, 65,504 kept and 65,520 refused by `fits_f16`, the tail
   bit-equal, wrong lengths and the exponent -128 refused) and
   `reader::tests::a_narrow_layout_needs_version_4_and_a_narrower_tail`: green.
2. Readers: `narrow::every_reader_returns_the_codecs_values_in_every_mode` (fixed `vector`,
   a named field and a varying multi-vector field, 60 rows, f32, f16 and i8: `read_fields`,
   `field_vectors`, `field_rows`, `field_spans`' ranges decoded by the layout, `vector_rows`,
   `vectors` and `scan`, each equal to the codec's values; version 3 for f32, 4 otherwise),
   `a_single_field_scan_decodes_its_inline_slices`,
   `f32_is_written_as_before_and_row_blocks_never_narrow` (f32 under the setting byte-equal
   to the default writer; row blocks under f16 and i8 byte-equal to f32's, version 1 or 2),
   `a_field_its_type_cannot_hold_stays_f32_in_that_segment` (f16 past 65,504, i8 past 2^127) and
   `a_corrupt_exponent_is_refused_not_misread`: green. The fixtures that pin segment bytes
   are unchanged and green.
3. `vector_storage::a_declared_type_its_storage_holds_answers_bit_equal_to_f32` (`[16]f16`
   under f16, `[16]i8` under i8 and under f16, each under dot product and euclidean: 240
   rows in two folds, the second narrow; a second process answers `include_vectors` and 20
   exact queries' ids and scores bit-equal to the f32 index) and
   `vector_storage::an_undeclared_or_cosine_field_answers_the_codecs_values_once_folded`
   (undeclared dot product and euclidean, declared f16 cosine, under f16 and i8: the narrow
   fold's rows answer the codec's rounding of f32's answer, more than half of them differ,
   the f32 fold's and the unfolded rows answer f32's): green.
4. `head::tests::vector_storage_overrides_round_trip_in_their_own_section` (alone and beside
   M231's settings, over three base HEADs; an index HEAD lacks writes nothing; codes 0, 4 and
   255, a name twice and a trailing byte refused), the unchanged M231 HEAD tests,
   `vector_storage::the_setting_takes_the_cluster_default_and_an_index_override` (the
   default seals f16; an override of f32 by a second server keeps M231's
   `block_aggregates`; cleared, that server's i8 default returns; versions `[4, 3, 4]`;
   every row reads back; under a `block_layout: rows` override the summary answers `f32`;
   `bf16` refused naming the setting, `16` and `true` refused) and
   `vector_storage::the_cluster_default_is_read_from_the_environment`: green.
5. Both modes: every existing test runs under the default, f32. Assertions changed: none
   weakened. The two full-object `storage` answers in `storage::an_index_overrides_the_cluster_default_and_says_so`
   gained the new `"vector_storage": "f32"` member, and struct literals of `BlockOptions` and
   `Storage` in tests gained the new field at its old-behaviour value. The workspace suite,
   without M127's two IPv6 tests the container cannot run: 3,048 passed, 0 failed.
   No request or round is added (no read path issues a new range), so `scripts/depth.sh` is
   not re-run here; CI runs it.
6. The bench, below: `cargo run --release -p pstore-index --example vector_storage`.

## The bench (criterion 6)

`cargo run --release -p pstore-index --example vector_storage`, **`provisional`**: a 4-core
cloud container, synthetic corpora, one segment in an in-memory store (so no network time:
the bytes saved are not seen as time). Default ANN parameters (p = 8, oversample, `rerank:
fast`), clustered at 1,000 rows. Recall is against exact f32 truth; "exact overlap" is the
exact search's top 10 against f32's exact top 10; the score error is the largest of a
returned row's score against its f32 score.

| Corpus | Mode | Vector bytes | Ratio | r@10 exact | r@10 ANN `rerank: exact` | r@10 ANN default | Exact overlap | Max score error | Exact scan ms | Build ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 20,000 x 384d clustered, cosine | f32 | 30,720,000 | 1.000 | 1.0000 | 0.9890 | 0.9720 | 1.0000 | 0 | 20.2 | 908 |
| | f16 | 15,360,000 | 0.500 | 0.9980 | 0.9880 | 0.9720 | 0.9980 | 5.5e-5 | 32.6 | 770 |
| | i8 | 7,700,000 | 0.251 | 0.9590 | 0.9490 | 0.9720 | 0.9590 | 2.4e-3 | 22.1 | 1,066 |
| 10,000 x 384d uniform, cosine | f32 | 15,360,000 | 1.000 | 1.0000 | 0.9510 | 0.9420 | 1.0000 | 0 | 9.6 | 357 |
| | f16 | 7,680,000 | 0.500 | 1.0000 | 0.9510 | 0.9420 | 1.0000 | 3.2e-5 | 14.3 | 486 |
| | i8 | 3,850,000 | 0.251 | 0.9830 | 0.9420 | 0.9420 | 0.9830 | 1.9e-3 | 8.8 | 503 |
| 20,000 x 384d clustered unnormalized, euclidean | f32 | 30,800,000 | 1.000 | 1.0000 | 0.9750 | 0.4140 | 1.0000 | 0 | 16.7 | 761 |
| | f16 | 15,440,000 | 0.501 | 0.9990 | 0.9750 | 0.4140 | 0.9990 | 3.3e-2 | 28.0 | 818 |
| | i8 | 7,780,000 | 0.253 | 0.9590 | 0.9440 | 0.4140 | 0.9590 | 1.13 | 16.5 | 1,082 |

(The euclidean corpus's default ANN recall is low in every mode: RaBitQ codes of an
unnormalized, extended vector are a poor first rung there. It does not move with the
storage mode, because the codes are built from the f32 vectors.)

### Downsides, with the numbers

- **Precision and recall, f16**: at most 0.002 of exact recall@10 and of exact overlap lost,
  none at the default ANN rung (which reads no raw vector) and at most 0.001 under `rerank:
  exact`. Score errors up to 5.5e-5 on cosine scores and 3.3e-2 on euclidean ones whose
  magnitude is in the hundreds.
- **Precision and recall, i8**: 0.017 to 0.041 of exact recall@10 lost, and up to 0.04
  under `rerank: exact`; default ANN unchanged. Score errors up to 2.4e-3 on cosine scores
  and 1.13 on euclidean ones whose magnitude is in the hundreds.
- **CPU, f16**: an exact scan of one segment takes **49% to 67% longer** (20.2 to 32.6 ms,
  9.6 to 14.3, 16.7 to 28.0): every component goes through M178's bit conversion. i8 is
  within 10% of f32 either way (22.1, 8.8, 16.5 ms). Build time moves by -15% to +42% (i8
  slowest, choosing an exponent a vector).
- **Euclidean distance is not the decoded vector's.** The f32 tail is `-|x|^2 / 2` of the
  vector as written, so a folded row scores `q.x' - |x|^2 / 2` over its rounded components
  `x'`: the reported distance is `|q - x'|^2 + |x|^2 - |x'|^2`, off by the rounding's change
  of norm (and possibly slightly negative before the clamp at 0). Exact where `x' = x`.
- **What a client sees**: a folded row's `include_vectors` and scores are the rounded ones,
  and they change at the row's fold (an unfolded row answers from the log, unrounded).
- **Rollback**: version 4 segments are unreadable before M289; the override is a trailing
  HEAD section refused before M289 (`docs/deploy.md`).

### The default

**`f32`, unchanged.** Criterion 6's bar for changing it: on every corpus no more than 0.002
of recall@10 and of exact overlap lost, and the exact scan no slower. f16 meets the
precision bar on all three corpora and fails the CPU one (49% to 67% slower in memory); i8
fails the precision bar on all three. Turn `f16` on for an index whose raw vectors' bytes
dominate: exact scans and `rerank: exact` over segments fetched from the store, and
projections of `include_vectors`, where half the bytes outweighs the decode, and whose
clients accept rounded scores. Turn `i8` on where a quarter of the bytes matters more than
about 0.04 of exact recall. Leave `f32` where answers must match what was written, unless a
`[N]f16` or `[N]i8` field under dot product or euclidean makes the narrow mode lossless
(criterion 3).

## Gates

`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`: clean.
`scripts/coverage.sh --changed --fail-under-lines 95 --fail-under-regions 95
--ignore-run-fail`, without the two IPv6 tests, after `cargo llvm-cov clean`: regions
96.16%, lines 97.48% over the changed crates; `narrow.rs` 99.19% of regions, `head.rs`
96.41%, `reader.rs` 93.42% (`provisional`: a 4-core cloud container). CI's coverage and
mutants jobs on the PR are the gate.
