# M74 — More than one clustered multi-vector field per segment

**Serves:** backlog row 77 (the second field). [M71](../M71/SPEC.md) clustered one field a segment
and left a second as a flat scan.

## Design

GC and replication derive a segment's sidecar keys from its key, and a LIST to discover per-field
keys is forbidden. So the fields share the one pair: `.fdec` becomes a **set** (`PFS1`: a count,
then per field its `FdeClusters` bytes and the byte offset of its vectors) and `.fdev` holds the
fields' vectors back to back. The `fde` leg looks its field up in the set; a field the set lacks
takes the flat scan. HEAD's one flag means "the set exists".

⚠️ **A migration, deliberately not written:** a `.fdec` in M71.2's single-field form (`PFC1`) is
read as damaged. M71.2 is hours old and unreleased; there is no such sidecar outside this branch.

## Acceptance criteria

1. `a_set_round_trips_and_refuses_damage` (`pstore-index`): encode/decode are inverse; truncated,
   wrong-magic, trailing-byte, a field twice and a count above the bytes are refused.
2. `each_field_searches_its_own_vectors`: two fields with different corpora in one set; a query
   against each returns that field's candidates, equal to a single-field sidecar's for it.
3. `two_fields_in_one_segment_each_use_their_clusters` (engine): a 9,000-document segment with two
   multi-vector fields has one `.fdec` naming both, and an `fde` leg over **each** reads fewer
   `.fdev` bytes than the flat scan of its field would.
4. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the new code misses 0.
