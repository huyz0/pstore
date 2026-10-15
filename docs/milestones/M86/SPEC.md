# M86 — Register an external index by name

**Serves:** backlog row 76. **Status: built, default off.** The owner decision below is still the owner's: nothing here turns the feature on for anyone, and it was built behind the default the spec planned.

## The decision this needs, and the default built

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

## What was decided in building it

- **The names are one namespace** with indexes and replications: registering a name any of them
  holds is `409 index_exists`. A registration is dropped before its name is reused.
- **The source's CAS tag lives in the codes blob, not in HEAD.** The codes carry the pinned
  column (key, length, tag), and a search compares them to the object -- the one place that can
  say `Changed`. HEAD carries the length too, so a reader of HEAD alone sees what was pinned.
- **The allowlist is the deployment's**, `PSTORE_EXTERNAL_PREFIXES`, not per tenant: there is no
  tenant configuration store to hold one. `allow` with no prefix is refused at startup, and a
  source containing `..` is refused whatever the prefix.
- **A query is bounded**: `k` at most 1,000 and `window` at most 10,000 rows, because the window
  is what a tenant's query costs the customer's bucket.
- **⚠️ Replication does not carry the registration** (the delta above says it copies it; it
  does not). A replica store has neither the customer's object nor the codes blob, and copying
  the codes needs a design this milestone did not make. Recorded as backlog row 79.

## Acceptance criteria

1. `register_then_query_by_name_equals_the_library`: registering a Parquet column and a Lance
   dataset and querying by index name returns the ranking the library's `ExternalIndex::search`
   does, to the bit; another handle on the tenant answers the same.
2. `an_unregistered_source_is_refused_unless_allowed`: with the default, a prefix that does not
   match, or no prefix at all, registration and query are refused and the counting store sees no
   request and no ranged read. Over the API: `external_sources_are_off_by_default` (`403`).
3. `a_changed_source_is_an_error_not_a_stale_answer`.
4. `head_with_externals_round_trips_and_an_old_head_decodes` (and
   `a_damaged_externals_section_is_a_corrupt_head`): the section encodes, decodes, and a HEAD
   without it decodes as no externals; an empty set changes no byte of the encoding.
5. `dropping_a_registration_buries_its_codes`: the graveyard names the codes at the dropping
   epoch, GC reaps them, the customer's object is untouched.
6. `a_registration_the_engine_cannot_honour_is_refused_with_its_reason`.
7. Over the API: `an_allowed_source_is_registered_queried_and_dropped_by_name`,
   `each_refusal_has_its_status_and_code`,
   `the_deployment_opts_in_by_name_and_names_its_prefixes`.
8. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Size:** L. Touches `pstore-engine` (`head.rs`, `external.rs`), `pstore-server` (`external.rs`,
`Config`), and one signature in `pstore-index` (`search_where`'s predicate is `Sync`, so a search
can be awaited in a handler).
