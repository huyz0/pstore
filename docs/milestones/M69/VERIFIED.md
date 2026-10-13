# M69 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store. Recall is on a seeded
synthetic corpus and **provisional**; no latency is claimed.

Command: `cargo test -p pstore-index --test external`: 6 tests.

1. `build_writes_codes_and_no_floats`: the codes blob is under an eighth of the 2,000-row
   column's bytes; none of 50 rows' first eight floats appears in it; a column holding a zero
   vector is refused.
2. `search_finds_the_exact_neighbours`: 20,000 × 64 clustered column in a file with a header, an
   8-byte record gap and a trailer; **recall@10 at `window = 200` is 0.95** (floor 0.9), every
   returned score equals the test's `f64` cosine to `1e-5`, results descend; the control (codes
   of another column over this object) reaches **0.01**.
3. `search_reads_only_the_candidates`: 100 scattered candidates move under half of the 5.3 MB
   object (2.2 MB measured: the coalescer fetches holes cheaper than a request). Seen red at a
   tenth, which the spec first claimed; the spec now states the measured behaviour.
4. `open_round_trips_and_refuses_damage`: a reopened index answers identically; a short, a long,
   a header-only, a wrong-magic, a wrong-dimension and an absent blob are each refused.
   `a_tagless_pin_round_trips_and_errors_say_what_they_are`: the pin without a CAS tag
   serializes and parses back; a bad flag byte is refused; each error kind names itself.
5. `a_changed_source_is_refused`: after the object is overwritten, `search` errors (M68's
   `Changed`).
6. **Gates:** `./scripts/gates.sh` green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants` over `external.rs`): first sweep 57 mutants, 2 missed: `Display` for the
   error (killed by the error-text assertions) and the `Some(0)` tag-flag arm (killed by the
   tagless round trip). Final sweep: **58 mutants, 49 caught, 7 unviable, 0 missed, 2 timeouts**
   -- `<` as `<=` and `+` as `*` in `build`'s chunk loop make it spin forever, which is detection.

**What OQ-169 now has:** an answer ([M68](../M68/SPEC.md)), a pinned read path (M68), and an
index over a customer's fixed-width column that writes codes only (this milestone).

**Not covered**, as the spec states: reading Parquet footers or Lance manifests to find `base`,
`stride` and `rows` (a caller supplies them), clustering, filters, deletes, HEAD naming the
index, an engine or API surface, and a source that changes (refused; rebuild).
