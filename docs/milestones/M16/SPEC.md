# M16 — `branch_from_namespace` and `copy_from_namespace`

## Serves

- OQ-33, branch refcounting under deep branch trees, which this milestone **answers**.
- [`mutations-and-mvcc.md`](../../research/05-storage-engine/mutations-and-mvcc.md) §
  Branching, and the parity row for both operations.

## What the corpus did not know

The corpus designed a branch as a new HEAD per index, so refcounting had to span HEADs. Here
**one HEAD per tenant names every index**, and GC refuses to reap any key HEAD still names.
- **OQ-33 needs no lineage object:** "named by HEAD" is an exact reference count at any depth.
- It needs three repairs instead. HEAD keys delete vectors by segment; a key can now be buried
  twice; and `as_of` attributes a buried key to the index its path names.

## Delta

- **Wire.** A write to `dest` of `{"branch_from_namespace": "src"}` must be **alone**:
  - no operations, `schema`, `distance_metric` or `durability`, each refused otherwise;
  - `copy_from_namespace` is the same operation, since a physical copy of immutable segments
    buys nothing (stated in the parity doc);
  - it answers the commit `epoch`, and a session token that saw it.
- **Names.** `src` and `dest` must match `[A-Za-z0-9_.-]{1,128}` and differ. This bounds key
  length, and makes the owner of a segment key exact: **`owner(seg) = key_index(seg)`**. An
  index **borrows** a segment it names but does not own. There is no stored set, so a past
  epoch answers by the same rule.
- **`dest` must not exist**, by the drop path's test: segments, schema rejects, unfolded rows,
  this process's pending rows. Nor may it be in `dropped`, where its past incarnation's
  history would become unreadable. **`src` must exist.**
- **What a branch holds.** `src`'s folded state at the commit. The server folds the tenant
  first, so every acknowledged durable write is in it.
  - A durable write to `dest` racing the commit lands on the branch, as any later write would.
  - `dest` takes `src`'s whole schema.
- **The commit** is one HEAD CAS, on the HEAD every input was derived from. It:
  - adds `dest`'s refs, which are `src`'s keys;
  - adds `dest`'s schema;
  - records `branched[dest] = epoch`.

  For each of `src`'s segments with a vector at `dv_ref(src, seg)`, it copies that vector,
  1 GET and 1 PUT, to `dv_key(scoped(dest, seg), epoch, lane)`. A copy made by an attempt
  that loses its CAS is **buried under its own key epoch**, as compaction buries its losers.
- **One delete-vector key per (index, segment):** `dv_ref(index, seg)` is `seg` when the index
  owns it, and otherwise `scoped(index, seg)` = `seg` + `.br-` + hex(index). Every read,
  insert and remove of `deletes` goes through it:
  - query targets, the count fast path, `index_stats`;
  - `prepare` and `supersede`, including the object key `supersede` writes;
  - compaction's lookup and burial, and the drop's.
- **Burying a borrowed segment** (the branch compacts or drops it) buries a **marker**,
  `scoped(index, seg)`, not `seg`. A marker names no object.
  - GC treats it as a burial of `seg`.
  - `as_of` resurrects `seg` under the marker's index.
- **GC** skips any key, or any marker's segment, that is still named by HEAD **or buried
  again in a graveyard entry newer than the horizon**. A doubly buried key is thus reaped only
  once both burials are past the window. Each key is reaped once per pass.
- **`as_of`** keeps these rules:
  - a resurrected segment goes to the marker's index, or else to `owner(seg)`;
  - a resurrected index lists no segment twice;
  - a scoped vector counts as live when its `(index, seg)` pair is live;
  - an index is absent at any epoch below its `branched` entry.
- **HEAD.**
  - It gains a trailing `branched` section. Whenever a later optional section is non-empty,
    **every earlier one is written, with a count of 0 if it is empty**, including M14's and
    M15's. A HEAD with no branch is byte-for-byte what M15 wrote.
  - `record_reap` prunes a `branched` entry at or below the horizon, as it does `dropped`.

**Does not change:** any other operation's requests. A query of `dest` costs what the same query
of `src` costs.

## Acceptance criteria

1. **Equal at birth.** Every kind of query of `dest` equals the same query of `src`, including
   rows `src` had deleted.
2. **Independent after**, in both directions. Each of these changes only its own index:
   - writes, deletes, patches and by-filter operations, including a delete in a shared segment;
   - `dest` compacted, then dropped, then `gc(0)`, after which `src` answers as before,
     including its deleted rows.
3. **GC-safe.** `src` compacted, and `src` dropped, each followed by `gc(0)`: `dest` answers as
   before. `dest` dropped then `gc(0)`: the shared objects are gone.
   - With `src` burying X at E2 and `dest` at E4, `gc` with a horizon in [E2, E4) leaves X in
     the store.
4. **Deep.** A branch of a branch of a branch, with deletes at each level, satisfies 1–3, and
   copies each level's vector from `dv_ref` rather than the plain key.
5. **History.** For both indexes, `as_of` at an epoch after the branch equals the query then,
   both before and after either index compacts or drops a shared segment. Below `branched`,
   `dest` is `404` and `src` is unchanged.
6. **Cost.** With nothing to fold and no contention, a branch of `s` segments, `d` of them
   with deletes, costs 1 HEAD read, `d` GETs, `d` PUTs and 1 CAS. A lost CAS buries its
   copies, as an interference test shows.
7. **HEAD.** It round-trips with default full text, no trigram section, and a non-empty
   `branched`.
8. **Refusals,** each `400` naming the rule:
   - `dest` exists or is in `dropped`;
   - `src` does not exist;
   - either name is outside the pattern, or the two are equal;
   - the branch shares the request with anything else.
9. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the field is refused | a vector not copied; the schema not copied |
| 2 | as 1 | any `deletes` site using the plain key: supersede, compaction, drop |
| 3 | as 1 | the newer-burial check removed; a marker not mapped to its segment |
| 4 | as 1 | copying `src`'s plain vector in place of `dv_ref(src)` |
| 5 | as 1 | a marker resurrected under `owner`; a scoped vector dropped by the liveness filter; `branched` unread |
| 6 | as 1 | a GET per segment; a loser's copy left unburied |
| 7 | the section does not exist | an empty earlier section omitted |
| 8 | as 1 | a refusal accepted |

## RA budget

A branch costs a fold, then 1 HEAD read, then `d` GETs, then `d` PUTs, then 1 CAS: a depth of
4 plus the fold's, off the query path. It scales with segments carrying deletes, which
compaction bounds, never with rows. Everything else is unchanged.

## Risks

- **Retained bytes.** A branch keeps a parent's segments alive after the parent drops them.
  Nothing reports those bytes: the corpus's "surface it" hazard stays open.
- **Mixed versions.** A pre-M16 process knows no markers and no scoped keys, so it would write
  a branch's deletes into the shared vector. An upgrade must reach every process before any
  branch exists, and no gate enforces it.

## Tasks

- **M16.1** — all of it: one change, because every piece is needed for any of it to be safe.
