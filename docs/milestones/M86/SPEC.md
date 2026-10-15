# M86 — Register an external index by name (PLANNED, not built)

**Serves:** backlog row 76. **Status: planned; blocked on one owner decision**, stated below.

## The decision this needs, and the default planned

Whether pstore's nodes may read a customer's own file at build time (and, for a query, at search
time). OQ-169 answered that it is *feasible*; it did not grant *permission*. The default planned:
**off.** A tenant registers an external source only if its deployment sets
`PSTORE_EXTERNAL_SOURCES=allow`; the read credential is the deployment's, read-only, and the key
prefix must match the tenant's allowlist. Nothing is read from a customer's bucket otherwise.
Change the default only on the owner's word.

## Delta

- HEAD gains a trailing optional section `externals`: per index, `{source key, source kind
  (parquet | lance), column, codes key, rows, source length, source CAS tag}`. Absent means none,
  as every earlier trailing section.
- `Engine::register_external(index, source, column)` pins the source (M69), builds the codes
  (`ExternalIndex::build`), writes them under a derived key, and commits HEAD naming them. A
  source that changes afterward is `Changed`, never a silently stale answer.
- `Engine::query_external(index, vector, k, window)` opens the named index and searches it.
- API: `PUT /v1/indexes/{name}/external` (body: `source`, `column`) and
  `POST /v1/indexes/{name}/external/query`; `400` for a shape the reader refuses, with its reason;
  `403` when the deployment has not allowed external sources or the key is off the allowlist.
- GC reaps the codes blob when the registration is dropped, through the graveyard like a segment's
  sidecars. Replication copies the registration, not the customer's file.

## Acceptance criteria

1. `register_then_query_by_name_equals_the_library`: registering a Parquet column and querying by
   index name returns the ranking the library's `ExternalIndex::search` does, to the bit.
2. `an_unregistered_source_is_refused_unless_allowed`: with the default, registration is `403` and
   no byte of the source is read (asserted by a counting store).
3. `a_changed_source_is_an_error_not_a_stale_answer`.
4. `head_with_externals_round_trips_and_an_old_head_decodes`: the new section encodes, decodes,
   and a HEAD without it decodes as no externals.
5. `dropping_a_registration_buries_its_codes`.
6. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Size:** L. Touches `pstore-engine` (`head.rs`, `lib.rs`, GC, replica), `pstore-server`.
