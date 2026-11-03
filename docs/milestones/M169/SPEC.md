# M169: a tenant's index count is capped, and checked at thousands

## Serves
M7b's row 19 decision (do not shard HEAD), which rests on "a tenant owns ~30,000 indexes, which
the product does not permit" ([M7b VERIFIED](../M7b/VERIFIED.md) criterion 10): nothing enforced
it. D-34 (an open's depth, unchanged at any count) and the "requests scale with nodes and bytes"
rule, checked at thousands of active indexes in one tenant for the first time.

## Delta
Today any write, create (M147), branch (M16), replication or copy (M22, M166) and external index
(M86) may add a name to a tenant's HEAD, without bound. M6i and M7b measured opens up to 5,000
indexes on the engine; nothing ran a server with thousands of *written, folded and queried*
indexes in one tenant.

After:
- **The limit.** A tenant's index count is the number of distinct names its HEAD holds in
  `indexes`, `schema_rejects`, `quarantine`, `externals` and `replications`. A request that would
  add a name past `limit` is refused; a name already counted is never refused by the limit.
  `limit = 0` is unbounded. Engine: `Engine::with_index_limit(n)`, unbounded by default. Server:
  `PSTORE_INDEXES_PER_TENANT`, **10,000 by default** (below M7b's ~29,700, above the thousands
  this milestone checks), `0` for none, a non-count refused at startup as `PSTORE_ENGINES` is.
- **Where it is checked.**
  - create, branch, replication/copy create and external attach: inside their existing HEAD read
    and CAS loop, against that HEAD, before any object is written. Exact: a create that loses its
    CAS to another create re-reads and is checked again.
  - a write (every upsert and patch through the door): against the door's cached HEAD names plus
    this process's unfolded new names. A name counted as new, at or past the limit, re-reads HEAD
    once before the refusal stands (a drop elsewhere may have made room), as M173's type check
    does. Never at the fold: a fold that refused would drop rows already acknowledged durable.
- **The refusal**: `EngineError::IndexLimit { limit }`, answered **409 `index_limit`** with a
  message naming the limit and that dropping an index makes room.
- **Not exact across processes**, and said so in `docs/deploy.md`: two servers whose cached
  HEADs both read `limit - 1` may each admit one new name by write; the HEAD then holds more than
  the limit, and every later new name is refused until drops bring it below. The overshoot is
  bounded by new names written concurrently on other processes before their next HEAD read.
- **The scale gate**: `scripts/index-scale.sh`, an `#[ignore]` server test run in release by CI's
  `recall` job (outside `cargo test` for `depth.sh`'s reason). One tenant, one server, at
  K = 50, 1,000 and 5,000 indexes: every index written durable, one fold, every index queried;
  then one index written and folded again among the K; then the limit set to K.

Not changed: HEAD's encoding, the fold, queries, the registry's engine cap. Not delivered:
sharding HEAD (M7b's decision stands), an exact cross-process count, per-tenant overrides.

## Acceptance criteria
1. Create (engine): with limit 3 and three indexes, `create_index` of a fourth is `IndexLimit`
   and HEAD is unchanged; a create of a name that loses its CAS to a third index's create is
   refused on the retry; after dropping one, the create succeeds.
2. Branch, replication and external (engine): at the limit, each refuses a new `dest` with
   `IndexLimit` and writes no object (the store's write count is unchanged).
3. Write (engine): at the limit, a write to a new name is `IndexLimit`; a write to an existing
   index is accepted; a process's unfolded new names count (limit 3, two folded, one new name
   buffered: a second new name is refused).
4. Stale door (engine): a second engine drops an index; the first engine's cached HEAD still
   counts it, and its write to a new name is accepted after one HEAD read. With no drop, the
   refusal costs exactly one HEAD read; an accepted write to a known name costs none.
5. Over the limit across processes (engine): two engines each admit a new name at `limit - 1`;
   both durable writes fold and are queryable, and a third new name is refused.
6. Server: `PSTORE_INDEXES_PER_TENANT` parses (default 10,000, `0` unbounded, a non-count refused
   naming the variable); a refused write and create answer `409` with code `index_limit`.
7. Scale gate, at K = 50, 1,000 and 5,000 (release, MemoryStore, `provisional` wall clock):
   a. every index's write is 200 and every index's query returns only its own rows;
   b. a query's `blob_reads` from a server that has not served the tenant are equal at every K,
      and `blob_lists` are 0;
   c. one index's durable write and fold among K: read and write request counts equal at every K;
      HEAD bytes written by that fold at most 256 B per index plus 4 KiB;
   d. with the limit set to K, a new name's write is 409 `index_limit` and an existing index's is 200.
8. Gates: fmt, clippy, suite, coverage 95/95, mutants on the diff, `scripts/index-scale.sh` green.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `index_limit::a_create_past_the_limit_is_refused` | check removed; `>=` as `>`; checked once outside the CAS loop |
| 2 | engine `index_limit::branch_replication_and_external_respect_the_limit` | a path left unchecked; checked after its objects are written |
| 3 | engine `index_limit::a_write_past_the_limit_is_refused` | existing names counted as new; unfolded names not counted |
| 4 | engine `index_limit::a_stale_door_rereads_before_refusing` | refusal from the cache alone; a read on every write |
| 5 | engine `index_limit::the_fold_never_refuses_an_admitted_write` | the limit applied at the fold |
| 6 | server `deploy::indexes_per_tenant_parses`, `index_limit::a_refusal_is_409` | default changed; error falling to 500 |
| 7 | server `index_scale::thousands_of_indexes_in_one_tenant` (`#[ignore]`, the gate) | a per-index request on a query or a fold; HEAD bytes per index growing |

## RA budget
Query: unchanged. Write: no request added while the name is known or the cache counts below the
limit; one HEAD GET when a new name meets the limit (refused, or admitted after a drop). Create,
branch, replication, external: none added (they already read HEAD). Fold: unchanged. List: 0.

## Risks
- A stale door under-counts: the cross-process overshoot above. Revealed by AC 5's HEAD count.
- A tenant already over a lowered limit keeps its indexes and may still write them; only new
  names are refused. AC 3's existing-index write pins it.
- 5,000 queries each reading a ~0.5 MB HEAD make the gate slow; it runs in release, in `recall`.

## Tasks
- M169.1: engine limit, the five checks, typed error, engine tests.
- M169.2: server config, error code, docs, scale gate and CI step.
