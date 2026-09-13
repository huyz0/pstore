# M6i — One million indexes, measured rather than extrapolated

**Serves:** M6's exit criterion in [`roadmap.md`](../../research/11-design/roadmap.md) — *"1M
indexes, open latency unaffected by index count, zero LISTs on any hot path"* — and
[`evaluation-methodology.md`](../../research/10-benchmarks-cost/evaluation-methodology.md)'s
*"Behaviour at 1M+ indexes: open latency, catalog enumeration time, memory per idle index"*.
⚠️ **Not OQ-72**, which asks for a *power-law* generator; this workload is uniform, so OQ-72
stays open and this spec must not be read as closing it.

## ⚠️ The gap is named in M6a's own ledger

> *"**Not measured at 1M.** The invariant is measured at 2,000 tenants; 1M is arithmetic on
> it."*

Arithmetic is what this milestone replaces. Every part the exit criterion needs now exists —
buckets ([M6a](../M6a/SPEC.md)), reaping ([M6d](../M6d/SPEC.md)), sweeping
([M6e](../M6e/SPEC.md)), splitting ([M6g](../M6g/SPEC.md)), re-partitioning
([M6h](../M6h/SPEC.md)), metering ([M6b](../M6b/SPEC.md)) — and none of it has been run at the
scale the design is argued at.

## ⚠️ Two different "index counts", and the exit criterion means both

The deployment holds 1M tenants × ~50 indexes. **Across tenants**, keys are derived, so a
deployment's size cannot reach the open path — that is the claim to confirm. **Within one
tenant** it is not obviously true: `Head.indexes` is a `BTreeMap<String, Vec<SegmentRef>>` in
a single object that is **read whole on every open**, so a tenant's own index count is in the
bytes of every one of its opens. Nothing has measured where that stops being free.

⚠️ **"An open" means `Engine::scan`**, named here because the engine has no operation called
open and two honest harnesses could otherwise measure different things: one whole-object read
of HEAD, then the segments the named index resolves to. Every stateful engine entry point
begins with that same HEAD read.

## Delta

- `crates/pstore-catalog/examples/scale.rs` — the OQ-72 workload. N tenants × K index names at
  `DEFAULT_WIDTH`, seeded through `Appender::observe`, folded, enumerated; reports per stage
  the resident memory, the wall clock, and the request count **by class**.
- `crates/pstore-engine/examples/open.rs` — open cost against a tenant's index count, and
  against the deployment's tenant count, as requests and as bytes.
- `scripts/scale.sh` — runs both. ⚠️ **Not a gate**, and not in `gates.sh`: the 1M arm peaks
  near 9 GB and takes about a minute, the same argument that keeps `recall.sh` and `depth.sh`
  outside `cargo test`.
- Whatever criterion 5 turns up, as a **test**, if it turns up a relationship worth pinning.

**Does not add** — **a 1M gate.** A CI runner that must hold 9 GB is a different problem from
measuring this once. **A fix for anything measured.** If open bytes scale with a tenant's
index count, this milestone reports the number and the knee; changing the HEAD layout is its
own spec with its own argument. **Real object storage.** A `MemoryStore` gives exact request
counts and exact bytes, which is what the criterion is about; latency against S3 is M0b.

## Acceptance criteria

1. **1M tenants exist, and are enumerated** — the workload seeds 1,000,000 tenants × 50 index
   names at `DEFAULT_WIDTH` and a census returns every one of them.
2. ⚠️ **Enumeration cost is a function of width, not of tenants** — the read count at
   1,000,000 tenants equals the read count at **200,000**, and both equal `2 × width + 1`
   exactly. ⚠️ 200,000 rather than something smaller because a census reads a run only for an
   occupied bucket: below roughly 160,000 tenants some of the 16,384 buckets are still empty
   and the arm would differ for a reason that has nothing to do with scaling. The harness
   asserts every bucket is occupied, so a too-small arm fails loudly instead of quietly.
3. **Zero LISTs on the cold paths and on the hot one** — seed, fold and census, **and every
   open in criteria 5 and 6**, asserted by the request-class counter and not by inspection.
   ⚠️ The roadmap says *"on any hot path"* and seed, fold and census are the cold ones; an
   open is the hot path, and two deployments that both LIST would satisfy criterion 6 without
   anything noticing.
4. ⚠️ **Peak resident memory is reported by stage, with the dominant term named.** A total
   alone hides which stage owns it, and the stage that owns it is the finding.
5. ⚠️ **Open cost against a tenant's own index count** — requests and bytes for one open at
   K = 1, 50, 500 and 5,000 indexes, reported whichever way they fall. Flat bytes would mean
   the HEAD is not read whole; growing bytes is the honest result and the knee is the number.
6. **Open cost against the deployment's size** — one open against a tenant in a
   1,000,000-HEAD deployment costs the same requests and the same bytes as in a 1-tenant one.
   ⚠️ **Structurally determined, and worth measuring anyway**: `Head::key` derives from the
   tenant id alone and nothing on the open path reaches the catalog, so this is *confirmed by*
   measurement rather than *discovered by* it, and the ledger must say which. ⚠️ The filler
   HEADs carry **distinct** payloads — a shared refcounted buffer cloned a million times would
   prove only that a hash map holds a million keys.
7. **Memory per idle index** — stored bytes per index, measured from the census. ⚠️ The other
   half, what a node holds for an index it is not serving, is **`NOT-RUN`**: it is zero by
   construction because nodes own nothing, no harness here instantiates a serving node, and a
   zero asserted by a harness with no node in it is evidence of nothing.
8. **`provisional`** and said so — WSL2, a `MemoryStore`, one run. Request counts and byte
   counts are exact; wall clock is relative only. Gates green.
9. ⚠️ **The verdict on M6's exit is recorded, per half** — in the ledger and in
   [`roadmap.md`](../../research/11-design/roadmap.md). Criteria 1–8 can all go green while
   *"open latency unaffected by index count"* is **unmet**, and a milestone that measures an
   exit criterion without scoring it has not finished the job.

## Test plan

⚠️ Criteria 1–8 are **measurements**, so their evidence is a number and the command that
produced it, in the ledger — the same shape as [M5i](../M5i/SPEC.md). What follows is what is
asserted **inside** the harness, so that a wrong run fails loudly rather than printing a
plausible number.

| # | Assertion, in the harness | The mutation it kills |
|---|---|---|
| 1 | the census length equals the seeded count | a fold that drops a bucket, which reads as a smaller and faster catalog |
| 2 | the two read counts are equal **and** equal `2 × width + 1` | equality alone passes if both arms are wrong the same way |
| 3 | the LIST counter is 0 after every stage | a LIST added on the fold path, which no functional assertion sees |
| 5 | the byte count is recorded per K, monotone or not | a harness that opens the same tenant every time and reports K's effect as noise |
| 6 | requests and bytes equal between the deployments, over **distinct** filler payloads | comparing requests only, which hides a HEAD that grew — and a shared buffer, which makes 1M tenants cost what one costs |
| 7 | — | a resident-memory zero that no node produced |
| 9 | — | eight green criteria and an exit criterion nobody scored |

⚠️ And one **test**, in `cargo test`, if criterion 5 finds bytes growing with K: a small
fixture pinning that an open reads the whole HEAD, so a later layout change has to face the
number rather than rediscover it.

## RA budget

Unchanged — nothing on a read or write path changes. The harnesses use a `MemoryStore` and
`Accounted`, and criterion 2 restates the existing budget rather than moving it: `width`
pointer reads + `runs` run reads + 1 root read, at depth 3. ⚠️ That is **M6a as amended by
[M6g](../M6g/SPEC.md)** — M6a's census was depth 2 with no root read, and the closing read
that refuses a census gathered at a width the deployment has left is M6g's.

## Risks

- ⚠️ **9 GB peak is close enough to this machine's free memory to matter**, and the census
  holding every record live is the term that grows. The harness reports RSS per stage, and the
  1M arm of each harness can be opted out of by environment rather than by editing a number.
  ⚠️ Criterion 6's 1,000,000 distinct HEADs are a second such budget, unrelated to the
  catalog's and unbudgeted until this amendment.
- **A `MemoryStore` cannot fail like S3.** Nothing here says what 1M tenants do under
  throttling; the claim is about counts and bytes, and the ledger says so.
- ⚠️ **Criterion 5 may embarrass the HEAD layout.** That is the point of measuring it. Fixing
  it inside this milestone would be scope creep, and reporting a knee that a later spec argues
  about is the honest outcome.

## Tasks

| Id | Commit |
|---|---|
| **M6i.1** | The OQ-72 catalog workload at 1M, with request classes and per-stage memory |
| **M6i.2** | Open cost against a tenant's index count and against the deployment's size |
