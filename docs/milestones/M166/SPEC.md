# M166: an isolated copy is an operation with a status

Spec review round 1 failed (two blocking: a finished copy kept the worker polling its
source, and a status is two reads, not one; six majors: an empty source, `paused`, old nodes
refusing the section, repeated-POST ordering, undelivered H items, length). Round 2 failed
on the repeated POST's match through `branched`, which GC prunes and a drop never clears:
an outcome is now removed when `dest` is dropped, and the match uses it alone. Round 3
passed; its minors are folded in.

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): an isolated copy
that outlives its source, as an async operation with a token, typed outcome, retention
and ambiguous-start recovery; and documenting `copy_from_namespace` as the branch it is.

## Delta
Today `copy_from_namespace` is M16's branch (shared segments, one tenant); M22's replication
copies a source's folded segments under dest's own keys, from any tenant or store the
principal may read (`Principal::source`), but follows it forever. After:
- **`POST /v1/indexes/{dest}/copy`**, a replication create's body, authorization and
  refusals, creates a replication with `once: true` and answers `202` with its status;
  `run`, the creating epoch, is the token.
- **Repeated POST**, matched on dest's HEAD before the source is read, source normalized
  (tenant and store defaulted), and again inside the create's CAS, after authorization (a
  revoked source is `403`): a running or paused copy from that source, or a `succeeded`
  outcome for it while `indexes` holds `dest`, answers `200`; else as a create does today.
- **Finish.** The CAS committing a once-replication's state also removes it, sets
  `branched[dest]` (dest is then an ordinary writable index), and records `copies[dest] =
  Succeeded {run, epoch, source_epoch}`: a sync commits a dest's whole state or nothing, so
  no partial success is published. The sync then clears its cached plan, and `Synced` names
  the finished copies, so the worker reconciles the register (an idle tenant is released).
- **Typed failure.** A source with no folded segment (absent, never folded, or an external
  index's name) at a sync's step 1 commits `Failed {run, epoch, code: "source_not_found"}`
  and removes the replication, only if its `run` and source still match inside the CAS.
  Every other error is retried and reported as `last_error`, never as a state.
- **Cancel** (M22's `DELETE .../replication`) records `Cancelled {run, epoch}`.
- **`GET /v1/indexes/{dest}/copy`**: `state` (`running`, `paused`, `succeeded`, `failed`,
  `cancelled`), `run`, `source`, `epoch`, `source_epoch`, `code`; `last_error` from the
  worker's notes for `running`/`paused` only, `null` otherwise. `404 copy_not_found`.
- **Retention.** At most 64 outcomes a tenant, a 65th evicting the smallest `epoch`. Dropping
  `dest` removes its outcome (`delete_index` changes), and creating a replication, copy or
  branch into `dest` replaces it (`create_replication` and `branch` change). No deadline: a
  copy runs until it succeeds, fails or is cancelled.
- **HEAD**: one trailing section, the once-names and `copies`; M22's section is unchanged.
- **Docs**: `copy_from_namespace` documented as a shared-segment branch, naming this route.
Not delivered from H: encryption policy (I), wall-clock times (epochs only), progress (a
copy is one commit), charging for transfer and duplicate storage; concurrency stays capped
by the worker's `max` and `IN_FLIGHT`. Not changed: queries and every format but HEAD's.

## Acceptance criteria
1. A folded multi-segment source with a delete vector, copied from another tenant and from
   a second store, is `succeeded` after one sync; dense top-k, filter and id queries answer
   `dest` as the source, and still do once the source is dropped and GC'd until its segment
   objects are gone (asserted) and the second store removed; a write to `dest` then folds.
2. The finishing commit made to lose once: inside the interference the status is `running`
   with no segment at `dest`; after the retry it is `succeeded`.
3. A source dropped before the first sync gives `failed`/`source_not_found` at step 1, no
   replication, no index at `dest`; a missing store stays `running` with a `last_error`.
4. Pause gives `paused`; cancel before the first sync gives `cancelled`, nothing copied.
5. A repeated POST is `200` with the same `run` while running, and after success even with
   the source dropped; another source is `409`, and after `dest` is dropped never `200`.
6. 65 finished copies keep 64 outcomes, the smallest `epoch` `404`.
7. After a finish the engine's next two syncs read no source HEAD; the server worker's
   `held` drops and the tenant's register entry has no `gen`.
8. A principal without the source tenant or store is refused before any source read.
9. HEAD round-trips the section; a HEAD without it encodes byte-for-byte as before.
10. A status is one round of two GETs; no LIST anywhere. Gates: fmt, clippy, suite,
    coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | server `a_copy_succeeds_and_is_independent` | finish not setting `branched` (write refused); outcome recorded but copy left running |
| 2 | engine `a_copy_never_reads_as_finished_partway` | outcome recorded outside the commit's CAS |
| 3 | engine `a_copy_of_a_missing_source_fails_typed` | failure path retried, or run check dropped |
| 4 | engine `a_paused_or_cancelled_copy_says_so` | pause reported as running; cancel without outcome |
| 5 | server `a_repeated_post_returns_the_same_operation` | match after the source read; outcome not removed at drop |
| 6 | engine `copy_outcomes_are_bounded` | eviction off by one, or largest evicted |
| 7 | engine `a_finished_copy_stops_polling`, server `a_finished_copy_releases_the_tenant` | plan not cleared; no reconcile after a finish |
| 8 | server `a_copy_is_authorized_on_the_source_side` | `Principal::source` skipped |
| 9 | `head.rs` unit test | section written when empty |
| 10 | server counting store in 1 and a status test | status reading the register shard |

## RA budget
A POST is M22's create, tail included, plus one dest HEAD read first (W1 CAS);
its syncs are M22's, with no request per document and no LIST, and a finish adds nothing
to its commit. A status is 2 GETs in one round. HEAD grows
by at most 64 outcomes (≤ ~10 KB at the 128-byte name limit).

## Risks
- A process older than M166 refuses a HEAD carrying the section (`CorruptHead`), as for
  every section since M22: upgrade every node before offering copies.
- A crash between a sync's copies and its commit leaks those copies, as any M22 sync's do.
  Nothing names them; GC never reaches them without a LIST.
- Pause and resume act on a copy as on any replication; `/v1/replications` lists it.

## Tasks
- M166.1: HEAD section, retention.
- M166.2: the finishing and failing commits, plan clearing, cancel's outcome.
- M166.3: routes, repeated POST, docs.
