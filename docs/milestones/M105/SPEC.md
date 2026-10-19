# M105 — OQ-6, answered: multipart and opaque tags, and a HEAD that cannot repeat

**Serves:** [OQ-6](../../research/00-plan/open-questions.md), "does the ABA nonce fully close
the hole under S3 multipart ETags?". The question was parked for want of a backend that makes
the `-N` and non-MD5 tag forms; [`blob-store-fakes.md`](../../research/09-rust-stack/blob-store-fakes.md)
§5 says to synthesize them. No backlog row: found by looking for blocked items that need
neither a real store nor a device.

## What is true today

These facts come from reading the tree at M104.

- **`MemoryStore` has two tag styles.** `Monotonic` behaves like a GCS generation. `ContentHash`
  behaves like the ETag of a single-part, unencrypted S3 PUT. Neither the multipart form
  `md5(concat(part md5s))-N` nor an SSE-KMS-style tag, which is not an MD5 of anything, exists
  anywhere.
- **No conditional write is multipart.** `put_conditional` in `object_store_backend.rs` is one
  PUT, and nothing in the tree calls a multipart upload. So the `-N` form never reaches an
  object we CAS. It would only arise if a caller started uploading a CAS'd object in parts.
- **Every CAS'd object, and why its bytes never repeat while the world they describe changes:**

  | Object | Why its bytes cannot return to an earlier value under different circumstances |
  |---|---|
  | tenant HEAD (`head::commit`) | every one of its 11 callers sets `next.epoch = at.head.epoch.next()`. **Nothing in `commit` checks it.** |
  | jobs shard | `version` is bumped by every write |
  | lane registry | a grow-only set: equal bytes are the same lanes |
  | roster | a grow-only union: equal bytes are the same members |
  | catalog bucket head | `run_epoch` advances at every fold, and the graveyard changes only at a fold (which buries, truncating to its cap) or a reap. Between folds only `pending` moves, and `merge` keeps the newest epoch per tenant, so `pending` can return to earlier bytes only by re-recording a tenant at the same epoch. Then no run object has changed, so the bytes are the whole state and a CAS landing on them is linearizable. `split` writes each sibling's head at `run_epoch` 1, conditioned on the tag it has just read, never a stale one |
  | catalog root | `epoch` "advances on a width change". `split` writes `root.epoch.next()`; `write_root` itself does not check that it advances |

- **The nonce adds no uniqueness the epoch does not.** `nonce_for(epoch, lane)` is a function
  of the epoch and the lane. Two HEADs at the same epoch from different lanes cannot both land,
  because both are conditioned on the same observed tag. So the property that closes ABA on a
  content tag is **"every HEAD write advances the epoch"**, and it holds only by convention at
  11 call sites. ⚠️ The corpus says otherwise: [`manifest-and-cas.md`](../../research/03-metadata-consistency/manifest-and-cas.md)
  §5 names "a random `commit_nonce`" as half of the ABA closure. The nonce was never random.
- **`commit_stale_for_test` is the 12th caller, and it does not advance.** It writes
  `Head::default()`, epoch 0, conditioned on `NotExists`. `engine.rs` and `linearizability.rs`
  assert only that it errs, and `divergent.rs` that it errs with `BackendCannotFence`.

## Delta

1. **`TagStyle::Multipart { part_bytes }`.** The tag is the FNV of the concatenated FNVs of
   each `part_bytes` slice of the body, in hex, then `-N` with N the part count (1 for an
   empty body). It is content-derived for a fixed split, as S3's is, so it is ABA-prone.
2. **`TagStyle::Opaque`.** Each write gets a fresh token that is unrelated to the content and
   is not a decimal number: the FNV of a per-store write counter, prefixed `kms-`. Equal content
   written twice gets two tags.
3. **`head::commit` refuses a HEAD whose epoch does not advance** on the one it was conditioned
   on (`Epoch::ZERO` when there was none). The check sits after `require_fencing` and before the
   PUT, and it returns `EngineError::Refused` with no request made. ⚠️ `Refused` is documented as
   a client's error, and this one is a bug in the engine. It is still `Refused`, because no
   caller can reach it: a new variant would be one more arm in every match for a state the
   suite shows unreachable.
4. **`commit_stale_for_test` writes epoch 1**, so its refusal comes from the CAS and never from
   the guard. The two tests that only asked whether it erred now ask for `EngineError::Lost`.
5. **The corpus is corrected.** `manifest-and-cas.md` §5 gets a banner: the nonce is
   `nonce_for(epoch, lane)`, and the epoch alone keeps HEADs distinct.

**Not changed:** the nonce. Removing it changes the HEAD encoding for no gain this milestone can
measure, and that is the owner's call. `write_root` is not guarded either: nothing serving calls
it outside `split`, which advances the epoch. The `object_store` adapter's ETag round trip is
not tested with a quoted `-N` or `kms-` tag: there is no `pstore-fake-s3`, and OQ-6's answer
says that half stays open.

## Acceptance criteria

1. **Multipart tags.** `MemoryStore::with_tag_style(TagStyle::Multipart { part_bytes: 4 })`:
   - a 10-byte body's tag ends `-3`, and an empty body's ends `-1`;
   - equal content gets equal tags;
   - 256 distinct bodies get 256 distinct tags;
   - after `v1`, `v2`, `v1`, a CAS on the first tag **lands**: the hazard is modelled.

   Observed red first against a stub that returns the `Monotonic` tag.
2. **Opaque tags.** `MemoryStore::with_tag_style(TagStyle::Opaque)`:
   - the same body written twice gets two different tags;
   - no tag parses as an integer;
   - after `v1`, `v2`, `v1`, a CAS on the first tag is `Lost`;
   - two clones of one store never hand out the same tag: the counter is in the shared state.

   Observed red first against a stub that returns the `ContentHash` tag.
3. **Conformance tells them apart.** `conformance::run` on `Multipart { part_bytes: 4 }` reports exactly one
   divergence, `aba_resistance`. On `Opaque` it conforms.
4. **The HEAD guard.** Through `commit_head_for_test`, a HEAD whose epoch equals the observed
   one, and one whose epoch is lower, each return `Refused`, and the stored HEAD is unchanged.
   Observed red first: today both land. `commit_stale_for_test` returns `Lost` in `engine.rs` and
   `linearizability.rs`, and `BackendCannotFence` in `divergent.rs`.
5. **The answer, on the engine.** A decorator records the body of every `put_conditional` that
   lands on the tenant's HEAD key. Under each of `ContentHash`, `Multipart { part_bytes: 64 }`
   and `Opaque`, a workload writes, folds, compacts, drops an index, recreates it with the same
   documents, runs GC, and then, from the engine that made the last commit before it, makes one
   `commit_head_for_test(|_| {})` that changes nothing. Every
   recorded body is **distinct**, and so is every tag the store returned for them. On a
   content-derived style, distinct bytes mean distinct tags, so no stale tag can match a HEAD
   that changed and changed back.

   Observed red first against a mutant of the tree with delta 3 absent and
   `commit_head_for_test`'s `next.epoch = next.epoch.next()` deleted. Its change-nothing commit
   then rewrites the HEAD it read, byte for byte: the nonce is derived from the epoch and the
   lane, and both are the ones it read. That is a HEAD repeating; it is not shown to be the only
   way one could.
6. **A stale tag under every style.** Under each of the three styles, and with real tags rather
   than `NotExists`:
   - `compact_with_interference_for_test` and `delete_index_with_interference_for_test` each
     hold a tag while another engine writes and folds a document with a new id: into the index
     being compacted, and into another index for the drop. (An id already in an input segment
     would make a delete vector, and `compact` then gives up rather than retries.) Each
     commit's first CAS is refused, counted by criterion 5's decorator, the retry lands, the other
     engine's document is still there afterwards, and the dropped index is gone.
   - 100 writers each write, flush and fold, and every acknowledged document survives.
7. **OQ-6 is answered** in `open-questions.md`, linking to this ledger, with the adapter half
   left open as delta's "not changed" says. The banner of delta 5 is in place.
8. Gates: `scripts/gates.sh`, and `scripts/mutants.sh` over the diff with no survivor.

## Test plan

| # | Seen red | What it catches |
|---|---|---|
| 1 | stub returning a monotonic tag | a multipart tag that is not content-derived, or carries the wrong `-N` |
| 2 | stub returning a content tag | an "opaque" tag that equal content can reproduce |
| 4 | `commit` as it is today | a HEAD write that does not advance the epoch |
| 5 | the change-nothing commit keeping its epoch, guard absent | a workload that repeats a HEAD's bytes |

## RA budget

Unchanged. The guard is a comparison before the request, and the tag styles exist only in
`MemoryStore`.

## Risks

- **A fake proves only what it models.** The `-N` and `kms-` forms here have the properties
  that matter to CAS: content-derived for a fixed split, and unrelated to content. Real S3 also
  changes a multipart tag when the same bytes are split differently. That could only cost a
  spurious `Lost`, which is liveness, not safety, and nothing CAS'd is multipart today.
- **The guard could refuse a legitimate write.** It would refuse any caller that meant to write
  the same epoch twice. None does; all 11 advance it, and the suite runs through every one.

## Tasks

- **M105.1** — the two tag styles, their tests and conformance.
- **M105.2** — the HEAD guard, `commit_stale_for_test`, the engine workload and the stale-tag
  runs under every style.
- **M105.3** — OQ-6's answer, the corpus banner, the ledger.
