# M281: one GC test across every kind of pin

## Serves
D-36's reap (M7e, M18, M201) and lesson 5 of
[lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md):
hsearch found a pin kind its GC forgot only when one test covered every kind together.

## Delta
1. **`head_keys(h)`**, the set of object keys HEAD names that GC must never reap, moved out of
   `reap_plan` into a function that **destructures every field of `Head`**. A field added
   later fails to compile there until someone decides whether it names an object.
2. **`reap_tests`**: one table of pins. Each row buries a key at epoch 1, applies its pin, and
   requires the plan through epoch 1 to keep the key, and to reap it without the pin. Rows:
   a live segment, a branch sharing the segment, a later burial of a borrowed segment inside
   the window, an `as_of` reader inside the window, a delete vector, a quarantine, a listed
   outcome, an external index's codes, and a replica's segment.
3. **An external index's codes blob is live.** Found by the table: `reap_plan` checked
   segments, delete vectors, quarantines and outcomes against HEAD, and not `externals`. The
   codes are buried by `commit_external` when its commit fails, including a failure whose
   write landed (an I/O error after the PUT, the case lesson 6 is about), and the next GC
   would then delete the codes HEAD names.

Not changed: what any commit buries, the window, the announce-delete-prune order. **No
setting**: a guard that only ever keeps an object HEAD names, so it has no downside.

## Acceptance criteria
1. Every pin keeps its key, and each key without its pin is reaped:
   `every_kind_of_pin_keeps_a_buried_key_and_nothing_else_does`.
2. With every pin at once, `head_keys` is exactly the keys they name:
   `every_key_head_names_is_live`.
3. Every existing engine test passes unchanged: `cargo test -p pstore-engine`.

## Test plan
Write the table first against `head_keys` with the old four sources; it must fail on the
external codes row and pass every other.
