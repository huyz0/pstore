# The Manifest: A Linearizable Register Made of One Blob

**Answers:** Q8
**Status:** Complete (v1)

This is the highest-risk, most novel part of `pstore`. Everything else has prior art; a
fully blob-resident, masterless metadata plane at fleet scale does not.

## 1. The primitive we have

A blob store with CAS gives us exactly one thing: **a linearizable compare-and-swap register
per key.**

```
CAS(key, expected_tag, new_bytes) -> Ok(new_tag) | Err(Lost) | Err(Contended)
```

Mapped per backend:

| Backend | Create-if-absent | Compare-and-swap | Failure |
|---|---|---|---|
| S3 | `If-None-Match: *` | `If-Match: <etag>` | 412 = Lost, **409 = Contended (retry)** |
| GCS | `ifGenerationMatch: 0` | `ifGenerationMatch: <gen>` | 412 |
| Azure | `If-None-Match: *` | `If-Match: <etag>` | 412 |

A linearizable register is enough to build everything we need. What it is *not* enough for
is high throughput: **~5 CAS writes/second per key** is the practical ceiling (turbopuffer's
published figure), because each attempt is a full round trip and losers must re-read and
retry. Under N contenders the success rate degrades further — classic OCC livelock.

> **The central design constraint of pstore:** the CAS register is *correct* but *slow*. It
> must therefore sit on the **structural** path (rare) and never on the **data** path
> (constant).

## 2. Fencing comes free — and that is the key insight

Kleppmann's argument: a lock alone never guarantees mutual exclusion, because a process can
be paused (GC, VM migration, network partition) past its lease expiry and then act. The only
fix is a **fencing token** that the *storage layer itself* enforces — "the storage server
remembers it has processed a write with a higher token and rejects lower ones."

Almost every distributed system has to bolt this on. **We get it for free**, because our
storage layer *is* the enforcement point:

> A zombie writer holding a stale manifest tag will have its CAS rejected with 412. It cannot
> corrupt state, no matter how long it was paused, because its write is conditioned on a
> version of the world that no longer exists.

Therefore:

> **Design rule 9. `pstore` has no locks and no leases for correctness.** Mutual exclusion is
> never assumed. Every mutation is (a) a write to an immutable, uniquely-named object, or
> (b) a CAS on the manifest. A paused, partitioned, or duplicated node is always safe.

Leases exist only as a **performance hint** to reduce duplicated work (see
`04-cluster/ownership-and-leases.md`), and losing one is never a correctness event.

## 3. Manifest structure

Copying Iceberg's proven skeleton (pointer → metadata → manifest list → manifests → data),
compressed to two levels because our commit rate is far higher:

```
HEAD (CAS'd, small, hot)          ~1–4 KB
  └── epoch, head_lane_watermarks[], manifest_ref, schema_ref, stats_digest
MANIFEST (immutable, per epoch)   ~100 KB – 50 MB
  └── segments[]: {id, level, shard, kind, byte_ranges, min/max, tombstone_ref, ...}
```

- **HEAD** is the only mutable object in an index. Key:
  `{h}/idx/{index_id}/HEAD`. It is small enough to read in one GET and CAS cheaply.
- **MANIFEST** is immutable and named by epoch: `{h}/idx/{index_id}/m/{epoch:020}.manifest`.
  Readers who already hold epoch *E* and see HEAD at *E* skip the fetch entirely.
- Every other object is immutable and content- or coordinate-addressed.

**RA(open index cold) = 2 Rseq** (HEAD, then MANIFEST). **RA(open index warm) = 1 Rseq**
(HEAD only, or 0 with bounded-staleness reads). **0 LIST, always.**

### Why not put the manifest inline in HEAD?
Because HEAD is CAS'd and re-read constantly by every reader of the index. Keeping it small
(a) makes CAS cheap and fast, (b) makes the contention window short, (c) lets readers poll it
for freshness without transferring megabytes. The manifest is fetched only when the epoch
actually changed.

### Large indexes: manifest chunking
Above a size threshold the manifest is split into per-shard manifest objects, with HEAD
listing their refs. A query touching one shard fetches one chunk. This keeps
`RA` flat as an index grows to a petabyte.

### Incremental manifests
Rewriting a 50 MB manifest per structural commit is write amplification we can't afford.
Instead HEAD points at a **base manifest + a short chain of deltas**:
`manifest_ref = {base: E0, deltas: [E1, E2, E3]}`, chain length capped (e.g. 8). When the cap
is hit, the next committer folds the chain into a new base. Readers fetch base + deltas **in
parallel** — 1 `Rpar` round, not `n` sequential ones.

## 4. The commit protocol

```
loop {
    (head, tag) = GET(HEAD)                       // 1 R
    if head.epoch >= my_view.epoch { rebase() }   // recompute against latest
    new_manifest = build()                        // pure, local
    PUT(manifest_key(head.epoch + 1), new_manifest)   // 1 W, immutable, idempotent
    match CAS(HEAD, tag, head.advance()) {        // 1 W
        Ok       => break,
        Lost     => { backoff_jittered(); continue }   // rebase and retry
        Contended=> { backoff_short(); continue }      // S3 409: retry, we did not lose
    }
}
```

Properties:
- **Linearizable.** The CAS totally orders structural commits.
- **Idempotent.** The manifest object is written at a key derived from the epoch we are
  attempting; a retry rewrites the same bytes at the same key, harmlessly. Losers leave an
  orphan manifest object which GC reaps (it is unreferenced by any HEAD).
- **Snapshot isolation for readers**, free: a reader holding epoch *E* has a complete,
  self-consistent immutable view forever (until GC).
- **Time travel and branching**, free: any epoch's manifest is still a valid root. Branching
  an index = writing a new HEAD pointing at an existing manifest. Copy-on-write forking is a
  single PUT.

### Contention management
Because ~5 CAS/s is the ceiling, we need contention *avoidance*, not just retry:

1. **Structural commits are rare by construction** (Pattern 10): data writes go to lanes,
   not HEAD.
2. **Deferred/coalesced commits.** A committer that loses the CAS checks whether the winner's
   commit subsumes its intent (e.g. another node compacted the same shard). If so it
   *abandons* rather than retries. This turns a retry storm into a single winner.
3. **Jittered exponential backoff with a contention estimator** — track observed CAS loss
   rate per index and widen the batch window accordingly.
4. **Per-shard HEADs for very hot indexes.** An index above a threshold gets HEAD split into
   `HEAD` (schema, shard map) + `HEAD/{shard}` (per-shard epoch). Independent shards then
   commit concurrently at 5 CAS/s *each*. The top-level HEAD changes only on reshard.

> This last point is how we beat turbopuffer's ~1 commit/s/namespace: **partition the
> register**. A 1,000-shard index has 1,000 independent CAS registers.

## 5. Hazards and how each is closed

| Hazard | Mitigation |
|---|---|
| **ABA** — tag reused for a different value | S3/Azure ETags are content-derived, so identical content ⇒ identical tag ⇒ CAS "succeeds" on a semantically different world. **Closed by:** HEAD always contains a strictly monotonic `epoch` and a random `commit_nonce`, so no two HEAD values are ever byte-identical. GCS generations are inherently monotonic and immune. |
| **S3 409 Contended** ≠ lost | Distinguish 409 from 412. 409 means "couldn't evaluate"; retry without rebasing. Conflating them causes needless rebase storms. |
| **Torn write** | Blob PUT is atomic; no partial visibility. Multipart completion is atomic at `CompleteMultipartUpload`. |
| **Orphaned manifest objects** from lost CAS races | Unreferenced by any HEAD; reaped by GC using epoch-range scanning (derived keys, not LIST). |
| **Lost HEAD** (catastrophic) | HEAD is rebuildable: epochs are dense and derivable, so DR walks `{epoch}` keys downward from a probe. This is one of the three sanctioned LIST use cases. |
| **Clock skew** | Nothing in the commit protocol reads a clock. Leases (perf hints only) do, and are allowed to be wrong. |
| **Read of a torn base+delta chain** | Deltas are immutable and named by epoch; HEAD names the exact set. A reader either sees the old HEAD (old chain) or the new (new chain). |
| **Thundering herd of 10K nodes polling HEAD** | HEAD polling uses **conditional GET with `If-None-Match`** — a 304 is cheap and transfers nothing; plus per-node jitter and a local TTL. See `consistency-model.md`. |

## 6. Why not a per-epoch new file instead of CAS (the Morling approach)?

Gunnar Morling's S3 leader election writes `lock_0000000001.json` per epoch using only
`If-None-Match`, because at the time S3 lacked `If-Match`. That works but **requires a LIST
to find the latest epoch** — exactly what we've banned. Now that `If-Match` exists, a single
mutable HEAD is strictly better: one GET to read the current state, no enumeration.

We keep the epoch-named-file idea for the *manifest* (immutable, addressable, cacheable) and
use CAS for the *pointer*. Best of both.

## 7. Cost

| Operation | RA |
|---|---|
| Read index state, cold | 2 Rseq |
| Read index state, warm (epoch unchanged) | 1 conditional Rseq → 304 |
| Read index state, bounded-staleness | 0 |
| Structural commit | 1 R + 2 W (+ retries) |
| Data write | **0** (lanes; see `05-storage-engine/write-path-and-wal.md`) |

## Open questions raised

- OQ-5: Measure real CAS throughput and loss-rate curves per backend under 2/8/64/512
  concurrent contenders. The "~5/s" figure needs our own numbers.
- OQ-6: Does the ABA nonce fully close the hole under S3 multipart ETags? Verify empirically.
- OQ-7: Optimal manifest delta-chain cap as a function of manifest size and commit rate.

## Sources

- [How to do distributed locking — Martin Kleppmann](https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html)
- [Locks, leases, fencing tokens, FizzBee! — Surfing Complexity](https://surfingcomplexity.blog/2025/03/03/locks-leases-fencing-tokens-fizzbee/)
- [The Fencing Gap: Why Your Distributed Lock Isn't Safe — HackerNoon](https://hackernoon.com/the-fencing-gap-why-your-distributed-lock-isnt-safe-and-how-to-fix-it)
- [Leader Election With S3 Conditional Writes — Gunnar Morling](https://www.morling.dev/blog/leader-election-with-s3-conditional-writes/)
- [Writing to an Apache Iceberg Table: How Commits and ACID Actually Work](https://amdatalakehouse.substack.com/p/writing-to-an-apache-iceberg-table)
- [How to build a distributed queue in a single JSON file on object storage — turbopuffer](https://turbopuffer.com/blog/object-storage-queue)
- [Building multi-writer applications on Amazon S3 using native controls — AWS](https://aws.amazon.com/blogs/storage/building-multi-writer-applications-on-amazon-s3-using-native-controls/)
