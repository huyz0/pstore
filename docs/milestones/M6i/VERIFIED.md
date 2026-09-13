# M6i — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Criteria 1–8 are measurements, so their evidence is a number and the command that
produced it** — `./scripts/scale.sh`, one run, quoted rather than paraphrased. The two tests
this milestone adds are in `cargo test -p pstore-engine --test open_cost`, and each "observed
red" below is a mutation applied to the shipped code, the test run, and the failure read.

1. **1M tenants exist and are enumerated** — `./scripts/scale.sh`: 1,000,000 tenants ×
   50 index names at the default width, seeded in 65.8s, folded across 16,384 buckets in
   10.1s, census returning **1,000,000 records** in 8.2s. The harness asserts the census
   length equals the seeded count, so a dropped bucket fails rather than reading as a smaller,
   faster catalog.
2. ⚠️ **Enumeration cost is a function of width, not of tenants** — `./scripts/scale.sh`:
   **32,769 reads at
   1,000,000 tenants and 32,769 at 200,000**: five times the tenants, the same requests, and
   both equal `2 × 16,384 + 1` exactly. Asserted against the closed form as well as against
   the other arm, because two arms wrong the same way are equal to each other. The harness
   also asserts all 16,384 buckets are occupied, so an arm too small to be comparable fails
   loudly.
3. **Zero LISTs, cold and hot** — `./scripts/scale.sh` asserts on the request-class counter after seeding, after
   folding, after the census, and on every open in criteria 5 and 6. Checked after the fold
   separately: a LIST on the fold path is invisible to a census that counts only its own
   requests.
4. ⚠️ **Peak resident memory by stage, and the dominant term named** — from the same
   `./scripts/scale.sh` run: seed 2.98 GB, fold
   3.58 GB, **census 9.28 GB**. The store holding every run is 3.58 GB; the census adds
   5.7 GB, because the answer holds all 1M records with their 50 owned `String`s each. **The
   census, not the catalog, is what costs.** That is not a harness artifact — a reader calling
   the census pays exactly this, and the incremental census is the existing way out.
5. ⚠️ **Open cost against the tenant's own index count — it grows, and here is the number.**
   Reads are **3 at every K**; read bytes go **7,903 → 13,097 → 60,797 → 537,797** at
   K = 1, 50, 500, 5,000. That is **~106 bytes of HEAD per index a tenant owns, paid on every
   open of every other index**, and at 5,000 indexes **98.6%** of an open is manifest. There
   is no knee: it is linear from 50 up. Pinned by
   `an_open_reads_the_whole_head_including_the_indexes_it_is_not_opening`, which asserts the
   **exact** byte difference is the manifest difference — observed red by reading HEAD twice
   in `scan`, which a `>=` assertion would have passed.
6. **Open cost against the deployment's size** — **3 reads and 13,097 bytes at 1, at 10,000
   and at 1,000,000 tenants**, identical. ⚠️ **Confirmed by measurement, not discovered by
   it**: the key derives from the tenant id and nothing on this path reaches the catalog, so
   the result was structurally determined and the ledger says so rather than dressing it up.
   The filler HEADs carry **distinct** payloads — 4.05 GB resident for the million — because a
   shared refcounted buffer would have proved only that a hash map holds a million keys.
   Requests are pinned by `an_open_costs_the_same_requests_however_many_indexes_a_tenant_has`,
   observed red by reading HEAD once per index entry.
7. **Memory per idle index** — **13.6 bytes per index** of catalog, identical at both arms
   (136,472,556 bytes over 10M indexes and 681,088,268 over 50M). ⚠️ The other half — what a
   node holds for an index it is not serving — is **`NOT-RUN`**: it is zero by construction
   because nodes own nothing, and neither harness instantiates a serving node. A zero asserted
   by a harness with no node in it would be evidence of nothing.
8. **`provisional`** — WSL2, a `MemoryStore`, one run; both harnesses print that line above
   their results. Request and byte counts are exact; wall clock is relative only.
   `./scripts/gates.sh` green. ⚠️ **How relative:** the run was repeated after an edit to the
   harness and every request count and byte count reproduced **exactly**, while the 1M seed
   went 65.8s → 39.5s on the same machine. The counts are the claim; the seconds are weather.
9. ⚠️ **The verdict on M6's exit, per half** — scored against the `./scripts/scale.sh` run
   above, recorded here and in [`roadmap.md`](../../research/11-design/roadmap.md).
   **1M indexes: met.** **Zero LISTs on any hot path: met.**
   **Open latency unaffected by index count: met across tenants, NOT met within a tenant.**
   Round trips are flat everywhere; bytes are flat across the deployment and linear in the
   tenant's own index count. **M6's exit is therefore not fully met**, and saying so is this
   criterion's whole job — eight green criteria and an unscored exit line is how a milestone
   closes without finishing.

## What this does not do, named rather than omitted

- ⚠️ **Nothing is fixed.** Criterion 5 found a real cost and this milestone deliberately does
  not spend it. Sharding HEAD per index would trade bytes for round trips — the budget that is
  currently *flat* — and that trade needs its own spec and its own measurement. What changes
  today is that a later layout argues with a number instead of rediscovering one.
- ⚠️ **This does not answer OQ-72**, and the spec was amended to stop implying it did. OQ-72
  asks for a **power-law** generator of index sizes and query rates; this workload is uniform,
  N tenants × exactly 50 names. It answers the roadmap's exit criterion, which is a different
  question that happened to be filed near it.
- **No real object storage.** A `MemoryStore` gives exact request counts and exact bytes, which
  is what the criterion is about. Latency under throttling at 1M tenants is M0b's, and blocked.
- ⚠️ **Not a gate**, and not in `gates.sh`. The catalog arm peaks at 9.28 GB and the pair takes
  about 90 seconds; a CI runner that must hold 9 GB is a different problem from measuring this
  once. The two tests it produced *are* in `cargo test`, at a fixture size that costs nothing.

## Process, recorded because it changed the spec

⚠️ **The spec failed its review with three majors, and the amendments are load-bearing.** The
reviewer found criterion 7 had no harness that could produce it, criterion 3 asserted zero-LIST
only on the cold paths when the roadmap says *"any hot path"*, and criterion 2's comparison arm
named no tenant count — at 20,000 the census reads fewer runs than there are buckets and the
arm would have failed for a reason unrelated to scaling. It also caught the OQ-72 mis-citation
and that the closing root read is M6g's rather than M6a's. ⚠️ The harnesses were being written
while that review ran, so the fix for criterion 3 was already in them by luck rather than by
design; the spec is what was wrong, and it is what was amended.
