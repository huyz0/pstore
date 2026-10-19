# M108 — the catalog splits when a bucket is full

**Serves:** OQ-8's remaining half, named in
[`roadmap.md`](../../research/11-design/roadmap.md) M6a: *"OQ-8's remaining half is the
**threshold** — what should trigger a split — not whether one can happen."* Also
[M6g](../M6g/VERIFIED.md)'s gap: *"automatic splitting: nothing measures bucket occupancy and
nothing calls `split`."* No backlog row.

## What is true today

These facts come from reading `pstore-catalog` at M107.

- `split` doubles the width. It reads the root, then every old head and run (two rounds, `2w`
  GETs), then reads each sibling's head again before writing it, then CASes the root. Nothing
  decides when to call it.
- **A run's bytes are what a bucket costs.** Every fold rewrites the bucket's whole run to
  admit at most `MAX_PENDING` (8) records, so a fold writes the run's bytes once per 8
  appends. A census reads every run, so it reads the same total at any width, and its request
  count is `2w`. Splitting halves the fold's write and doubles the census's requests.
- **Two splits at once can lose a record.** A split writes a sibling's head conditional on the
  tag it read there, and an absent head or an existing one alike gets overwritten with the
  split's run and no pending. Consider two splitters A and B that read the same root. A
  finishes and flips the root. An appender at the new width records tenant T into sibling `s`
  as a pending record. B then reads `s`'s head and overwrites it. T is in no bucket, and the
  root CAS that B loses afterwards undoes nothing. With `split` called by hand this needed two
  operators at once. With every node deciding on its own, it is the expected case.

## The threshold, and why

**Split when the fullest bucket's run exceeds `max_run_bytes`, default 2 MiB.** The fullest,
not the mean: the fold that costs most is the fullest bucket's, and hash skew is a factor this
milestone measures rather than assumes. The 2 MiB comes from the corpus's own design point.
[`catalog-without-master.md`](../../research/03-metadata-consistency/catalog-without-master.md)
§2 sizes 1M tenants of ~10 KB at `DEFAULT_WIDTH` as 640 KB per bucket, mean, and accepts it.
Hash skew puts the fullest bucket at 1.62× the mean there (measured before writing this, and
pinned by criterion 1), so the fullest bucket is ~1.06 MB. The default is about twice that:
the design point does not split, and a deployment that has grown past twice its design size
does.

⚠️ **An argument, not a measurement of cost.** What a real fleet's record sizes are is the
other half of OQ-8 and needs a real deployment. The threshold is a parameter for that reason;
the default is the design point's.

## Delta

1. `pstore_catalog::Occupancy`: the width, each bucket's run bytes, and the fullest bucket
   with its bytes. It is measured from the heads and runs a split reads anyway, so measuring it
   costs one census (the root, then `2w` GETs in two rounds, no LIST) and nothing per tenant.
2. `pstore_catalog::SplitPolicy { max_run_bytes }`, `Default` at 2 MiB.
3. `pstore_catalog::split_if_due(store, &policy) -> Result<SplitOutcome, _>`:
   - `NotDue(Occupancy)` when the fullest run is within the threshold: one census, no write;
   - `Split { from, to }` when it is over: the split reuses the census it decided on, so a
     due split costs no extra read round;
   - `AtFormat(Occupancy)` when it is over and the width is already `MAX_WIDTH`. That is
     reported, not an error: the deployment is full, and that is the operator's question.
4. **A split never overwrites a sibling head that exists.** It writes each sibling's head
   create-if-absent, without reading it first, and a lost create is success. An existing head
   was written by a concurrent split that already moved these records, by an appender at the
   new width, or by a crashed attempt of this split. A crashed attempt's run may be older than
   this one's, and that loses nothing: the old bucket keeps every record because nothing
   prunes, and a new-width census reads it and merges by tenant, newest epoch winning. The run
   written before a lost create is garbage nobody points at, the same as a run a lost fold
   leaves. `split` itself gets the same rule, since `split_if_due` calls it.

**Not changed:** who calls it. Nothing in the workspace depends on `pstore-catalog`, and that
is how "a tenant open issues zero catalog requests" is enforced (the crate's own docs). A
periodic caller is the same maintenance loop `fold`, `reap` and `sweep` are waiting for. Also
not changed: pruning, shrinking, `MAX_WIDTH`.

## Acceptance criteria

1. **The default threshold, against the design point.** The fullest bucket's tenant count is
   measured on `bucket_of` alone over `DEFAULT_WIDTH`, and printed with its skew over the mean.
   Records are sized so the mean bucket at 1,000,000 tenants is 640 KiB, 10,737 bytes each:
   - at 1,000,000 and at 2,000,000 tenants the fullest bucket is under the default;
   - at 2,500,000 it is over.

   This is arithmetic on the measured counts, not a written store. Criterion 3 is where the
   threshold is exercised against real runs.
2. **Below the threshold nothing is written.** On a catalog of 400 tenants at width 4,
   `split_if_due` with a threshold above the fullest run returns `NotDue`. It reports the
   fullest bucket and the bytes that a direct read of each run gives. It costs exactly `1 + 2w`
   reads (the root, every head, every run), 0 writes and 0 LISTs, and the root is unchanged.
3. **Above it, one split, and no extra read round.** The same catalog, with a threshold one
   byte under the fullest run, returns `Split { from: 4, to: 8 }`. The root says 8, and every
   tenant survives an enumeration at 8. The reads are exactly the census's `1 + 2w`: the
   decision and the split share it, and a sibling head is created without being read first
   (delta item 4). Seen red against a stub that always answers `NotDue`.
4. **At the format, it says so.** At `MAX_WIDTH` with a 1-byte threshold it returns
   `AtFormat`, and the root is unchanged.
5. **A record appended between two splits survives.** A store hides the flipped root from
   splitter B, so B reads the old root after A has split. Tenant T is then recorded at the
   new width into a sibling bucket, and B runs. B's split fails on the root, T is found by an
   enumeration at the new width, and every other tenant is too. Seen red against today's
   `split`: T lost.
6. **A re-run after a crash still completes.** The sibling heads are written and the root is
   not flipped, as a crash before the root CAS leaves them. A second `split` then flips the
   root, and every tenant is found. M6g's criterion 8 still holds.
7. **Zero LIST** across criteria 2–6.
8. **The roadmap's line and M6g's gap** are struck, linking here.
9. Gates: `scripts/gates.sh`, and `scripts/mutants.sh` over the diff.

## Test plan

| # | Seen red | What it catches |
|---|---|---|
| 1 | — (it is a measurement; criterion 3's red is the threshold's) | a default that splits at the design point, or never |
| 2, 3 | a stub `split_if_due` that never splits | a policy that is never consulted |
| 5 | today's `split` | a concurrent split wiping a new-width append |
| 6 | — (a regression guard for M6g's resume) | the create-if-absent rule refusing its own crashed attempt |

## RA budget

`split_if_due` costs one census when not due: the root, then `2w` GETs in two rounds, the same
as an enumeration, and it runs at maintenance cadence, never on a request path. Requests scale with
the width, never with tenants.

## Risks

- **A threshold on the fullest bucket splits the whole deployment for one hot bucket.**
  Splitting is all-or-nothing by design (M6g's doubling), so a skewed bucket costs every
  bucket a split. Criterion 1's skew number is what says how far the fullest bucket is from
  the mean.
- **Each split doubles the bucket objects and reclaims nothing** (M6g), and `MAX_WIDTH`
  allows two doublings from the default. `AtFormat` is how the third is reported.

## Tasks

- **M108.1**: `Occupancy`, `SplitPolicy`, `split_if_due`, the sibling rule, the tests, the
  ledger.
