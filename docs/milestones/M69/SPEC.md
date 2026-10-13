# M69 — An index over vectors left in the customer's file

**Serves:** OQ-169's question itself — *can an index be built over a customer's own files,
without copying the vectors?* — after [M68](../M68/SPEC.md) delivered the pinned read path.
This milestone builds the index and searches it, for the files M68 says qualify: fixed-width,
uncompressed `f32` columns (Lance; uncompressed PLAIN Parquet whose column offsets the caller
supplies). Parsing Parquet footers or Lance manifests to find `base`, `stride` and `rows` stays
out: a caller names them, and M68 pins the object.

## Delta

`pstore_index::external::ExternalIndex`:
- `build(store, codes_key, column, chunk)`: reads the column `chunk` rows at a time by ranged
  read, normalizes each vector, encodes it as a 1-bit RaBitQ code, and writes **one blob of
  codes** at `codes_key` — a header (dims, rows, the pin: key, base, stride, len, tag) and
  `rows` codes. **No float of the customer's data is written.** A zero vector is refused
  (cosine has no direction for it).
- `open(store, codes_key)`: reads the blob (one request) and rebuilds the index, refusing a
  truncated, mislabelled or wrong-length blob.
- `search(store, query, k, window)`: scores every code (the estimate), keeps the `window` best,
  reads **only those rows** from the customer's object (one coalesced ranged read, M68's, which
  refuses a changed object) and returns the top `k` by **exact cosine**, descending, ties by
  row.

Depth: the codes blob (round 1) then the candidate rows (round 2): two rounds, the second the
opt-in exact rerank. Bytes: `rows × code_len` once, then `window × dims × 4`.

**Not covered:** clustering (a flat scan of codes), filters, deletes, HEAD naming the index, an
engine or API surface, a source that changes (it is refused; rebuilding is a new blob).

## Acceptance criteria

1. `build_writes_codes_and_no_floats`: the blob's size is its header plus `rows × code_len`,
   under a tenth of the column's bytes at 256 dims, and contains no run of the column's floats.
2. `search_finds_the_exact_neighbours`: on a seeded clustered 20,000 × 64 column in a file with
   a header, a record gap and a trailer, **recall@10 of the exact cosine top 10 at
   `window = 200` is at least 0.9 (measured 0.95 on the first run, provisional)**, the returned
   scores equal the test's own `f64` cosine to `1e-5`, and a control (codes of a different
   column) falls below the floor.
3. `search_reads_only_the_candidates`: a search of 100 candidates over a 5 MB object moves under
   half of it: the candidates are scattered, so the read coalescer fetches holes cheaper than a
   request, and the codes' read is not the customer's object at all.
4. `open_round_trips_and_refuses_damage`: a reopened index answers identically; truncated,
   wrong-magic, wrong-dimension and wrong-length blobs are refused.
5. `a_changed_source_is_refused`: after the column's object is overwritten, `search` is an
   error (M68's `Changed`), and `build` of a zero vector is refused.
6. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the new module misses 0, or
   each miss is killed by a named test.
