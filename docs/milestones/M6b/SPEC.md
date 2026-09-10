# M6b — Quotas and metering: the counter that already exists, made to refuse

**Serves:** **Design rule 13** ("every resource that can be consumed on behalf of a tenant must
be metered per index — CPU, cache bytes, and especially **blob requests**, because that is the
one that shows up on the bill"), the per-tenant token buckets in
[load-and-hotspots](../../research/04-cluster/load-and-hotspots.md) § "Isolation between
tenants", and **D-64**. Opens no new id space.

**Depends on** [M6a](../M6a/SPEC.md), which built the catalog a billing rollup enumerates, and
on `pstore-blob`'s `Accounted`, which has counted per tenant since M0a.

## ⚠️ Why this is its own milestone, and what it is not

M6a took the roadmap's M6 apart and left this half named. The two share a subject and nothing
else: the catalog is an object layout, and metering is a decorator with no catalog in it.

⚠️ **This is not the 1M-index workload.** The roadmap's third M6 bullet is a synthetic
benchmark, and M6a's criterion 4 already measured the invariant it exists to check —
enumeration cost is a function of width, not tenants. What is left of that bullet is an
extrapolation, not a milestone.

## ⚠️ Per **tenant**, where the rule says per **index** — declared, not smuggled

Design rule 13 says "metered per index", and load-and-hotspots asks for "per-index token
buckets". This meters per **tenant**, because it sits on `BlobStore` — the only place a request
cannot escape — and that trait has no index concept: `Accounted` keys on `TenantId`, and a blob
key is a string. The consequence is real and is the thing the rule is about: **one index can
starve the other forty-nine in the same tenant**, and this milestone does not stop it. Metering
per index needs the meter above the trait, where every future caller has to remember it — which
is the trade, and it is the corpus's conclusion being narrowed rather than met.

## ⚠️ The failure this milestone exists to prevent

**The counters are already there, and nothing reads them.** `Accounted` has counted requests
and bytes per tenant since M0a; every RA assertion in this repository is built on it. What has
never existed is anything that *acts* on a number — so one tenant can issue unbounded blob
requests on behalf of a query, and the only place that shows up is the bill, a month later,
attributed to nobody.

⚠️ **The second failure is the shape of the fix, not its absence.** A quota enforced by
counting after the fact refuses the *next* request, which is exactly one request too late on
the operation that matters: a fan-out of 4,000 ranged reads is one decision and thousands of
requests. So the **request** reservation is taken for the whole operation, before any of it is
issued — the same argument D-63 makes for scan bytes.

⚠️ **Bytes cannot work that way, and pretending otherwise is the trap.** Transferred bytes are
unknowable before the transfer, and for `get` there is not even an upper bound available
without a `head` — which `store.rs` forbids on the read path in as many words. So the two
buckets settle differently, and the difference is stated rather than blurred:

| | admitted on | debited on |
|---|---|---|
| **requests** | the balance *before* dispatch, for the whole fan-out | the same number, at reservation |
| **bytes** | the balance *before* dispatch, whatever it is | **what was actually transferred**, after — **going negative** if it overran |

So a byte quota refuses the *next* operation, not the one that overran — one operation too
late, deliberately, because the alternative is a `head` on every read or an estimate the
substrate cannot make. Requests are the resource that reaches the bill (Design rule 13) and
they are the one bounded exactly; bytes are bounded within one operation's overrun.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| Metered resources | **blob requests and blob bytes**, per tenant | Design rule 13 names CPU, cache bytes and blob requests; the first two need a scan budget and a cache that reports per tenant, and neither exists yet. Requests and bytes are what `Accounted` already measures **exactly**, and requests are the one that reaches the bill. |
| Refusal | **`429`-shaped**, never a silent degrade | `api-design.md` makes every tradeoff a client parameter and every response report its cost. A quota that silently returned fewer results would be a wrong answer at a lower price — the failure M5d's preamble names, in a different subsystem. |
| Window | **A token bucket**, rate + burst | load-and-hotspots asks for exactly this. A fixed window lets a tenant spend a whole period in one instant and then starve; a bucket prices the burst. |
| Where it sits | A **decorator over `BlobStore`**, outermost above `Accounted` | The only way pstore touches durable storage is this trait, so the trait is the only place a request cannot escape the meter. Enforcing in the engine would leave the catalog, the roster and every future caller unmetered. |
| A fan-out's request cost | the **coalesced fetch count**, not the range count | `get_ranges`/`get_ranges_as` are *provided* methods that call `coalesce(ranges, capabilities().coalesce_gap)` and issue `plan.len()` requests. Reserving *n* would refuse fan-outs that were inside the quota, and would disagree with `Accounted` beneath on every coalesced read. `pstore_blob::coalesce` is public; the meter uses it. |
| An overrun's debt | the byte balance **goes negative and stays refused until refill clears it** | ⚠️ Saturating at zero would let a tenant repeat "one giant read per refill tick" forever, so the byte quota would bound nothing over time — which is the one thing a bucket exists to do. "Admits one overrun" is only true if the debt persists. |
| The byte figure | **what `Accounted` bills** — the merged buffer, gap bytes included | A coalesced fetch really does move the bytes between the ranges. Charging the sum of the slices handed back would undercharge exactly the reads the coalescer exists to make, and would make criterion 8 unsatisfiable. |

## Delta

**Adds**
- **`pstore-meter`** (layer 1, beside `pstore-blob`), depending on `pstore-blob` and
  `pstore-types` only.
- `Quota { requests_per_sec, request_burst, bytes_per_sec, byte_burst }`, and `Meter` — a
  token bucket per `(tenant, resource)`, refilled from a caller-supplied clock so a test is
  not a sleep.
- `Metered<S>` — a `BlobStore` decorator that reserves before it dispatches and refuses with a
  `429`-shaped message naming the tenant and the resource: `BlobError::Other` on the read and
  write paths, and ⚠️ **`CasError::Io`** on `put_conditional`, whose signature has no other
  home for it. Not `Contended` — every caller in the tree treats `Contended` as "retry the same
  attempt unchanged", which would turn a quota refusal into a retry storm against the quota.
  ⚠️ **Both fan-out entry points**, and they are not the same one: the dense read path uses the
  *unhinted* `get_ranges` (`format::reader`, `index::vec_index`), while only the text, sparse
  and cache paths use `get_ranges_as`. Overriding one leaves the other reserving per range
  inside the loop — the exact mutation criterion 3 exists to kill, alive on the larger fan-out.
  `Metered` also forwards `get_range_as`, `get_suffix_as` and `get_immutable`, for the reason
  `store.rs` gives and `class_forwarding.rs` now tests.
- ⚠️ **`get_tag` is metered and never refused.** It returns `Option<CasTag>` with no error
  channel, and `None` means "absent" — on the rebase step of the CAS commit protocol, which
  would turn a refusal into a `Precondition::NotExists` write against an object that exists.
  A silent wrong answer, so the meter counts it and admits it. Named here because it is a hole.
- `Meter::usage(tenant) -> Usage { requests: [u64; 4], bytes: [u64; 4] }`, per `OpClass`, in
  `pstore-meter` — the same shape `Accounted` keeps, so criterion 8 is a comparison rather
  than a conversion.

**Does not add** — ⚠️ **`Usage` in `TenantRecord`, which the first draft had and spec review
removed.** It cannot work as a field: `TenantRecord::identity` covers state and index names, and
`Appender::observe` writes nothing when identity is unchanged. Put `usage` *in* identity and
every observe appends — a blob request that scales with **elapsed time per tenant**, an
AGENTS.md "Never", and M6a's `an_unchanged_index_set_writes_nothing` goes red, which is not a
test to weaken. Leave it out and the stored number is frozen at the tenant's last lifecycle
event, so the rollup it exists to feed aggregates nothing. And `merge` is newest-epoch-wins, so
a per-node meter's number would be **last-writer-wins across nodes rather than summed**. It is
its own milestone with its own format question — the catalog's records carry no version field
at all, so a longer record cannot be told from an older one.

Also not added: **CPU or scan-byte accounting** (D-63/D-64): it needs a planner estimate that
does not exist; **cache-byte quotas** (OQ-21): `pstore-cache` does not report per tenant;
**the billing rollup**, which has no home until there is a server; **shedding policy under
overload** — D-63's degrade-or-shed choice is a query-layer decision and this is a substrate
one; **wiring `Metered` into any stack**. ⚠️ M6a's ledger said `Appender::observe`'s "caller is
M6b's"; it is not, and that is recorded here rather than left as two ledgers disagreeing —
the caller is whoever composes a serving stack, and nothing does yet.

## Acceptance criteria

1. A tenant inside its quota is **unaffected**: byte-for-byte identical results and the same
   request count as the undecorated store, asserted on `Accounted` beneath.
2. A tenant that exceeds its request quota is refused, and the error **names the tenant and
   the resource**; a second tenant is unaffected in the same `Meter`.
3. ⚠️ **The reservation is taken for the whole operation, and it counts coalesced fetches.**
   A `get_ranges` **and** a `get_ranges_as` whose *plan* would exceed the quota issue **zero**
   requests, not `plan.len()` − 1, asserted on `Accounted` beneath the meter. The fixture uses
   `MemoryStore::with_coalesce_gap(0)` so ranges do not merge and the plan length is the range
   count — ⚠️ at the default 64 KiB gap every range in a small object merges into one fetch,
   and the test could not tell reserve-once from reserve-per-range. ⚠️ **A second fixture, at a
   gap that does merge**, is what separates "reserve the plan" from "reserve the range count":
   at gap 0 those two numbers are equal and the mutation is invisible.
4. Tokens refill at the configured rate from a **supplied clock**: after advancing the clock by
   one second, exactly `requests_per_sec` more requests are admitted — no sleeping, and no
   dependence on how long the test took.
5. The bucket is bounded by its burst: advancing the clock by an hour does not admit more than
   `request_burst` in one instant.
6. Bytes and requests are **independent**: a tenant with bytes exhausted and requests available
   is refused for bytes, with an error naming bytes.
   ⚠️ And an overrun **persists**: a tenant whose read took it past zero is still refused on the
   next operation, and is admitted again only once refill has cleared the debt. A bucket that
   saturated at zero would pass every other criterion here and bound nothing over time.
7. ⚠️ **Bytes are debited on what was actually transferred**, which is the figure `Accounted`
   bills: a ranged read that returns fewer bytes than requested bills the smaller number, a
   refused request bills nothing, and a coalesced fetch bills the merged buffer including the
   gap bytes it really moved.
8. `Meter::usage` reports the same totals `Accounted` observed beneath it **for operations
   that succeeded**, so metering and accounting cannot disagree about what a tenant spent.
   ⚠️ Scoped deliberately: on a fan-out that fails partway, `try_join_all` returns `Err` while
   the fetches that did return have already been billed underneath, so the two legitimately
   differ on the error path and an unscoped claim would be false.
9. `get_tag` is counted and never refused, even for a tenant with no tokens left — the hole,
   pinned so it cannot close by accident into a silent `None`.
10. Region coverage ≥95% on `pstore-meter`, mutation ≥80%, full gate set green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_tenant_inside_its_quota_is_untouched` | a meter that rounds, batches, or drops a request it admitted |
| 2 | `an_exhausted_tenant_is_refused_by_name`, `one_tenants_quota_is_not_anothers` | a single global bucket — which passes every single-tenant test |
| 3 | `a_fan_out_that_would_exceed_the_quota_issues_nothing` (both entry points), `a_coalesced_fan_out_reserves_what_it_will_actually_issue` | reserving per range inside the loop — `plan.len()` − 1 requests land and the last is refused, and criterion 2 still passes; and reserving the *range* count, which refuses fan-outs that were inside the quota |
| 4 | `tokens_refill_at_the_configured_rate` | refill from wall-clock time, making the test's own duration the input |
| 5 | `a_long_idle_does_not_admit_more_than_the_burst` | an unbounded accumulator — a tenant idle overnight then issuing a million requests at once |
| 6 | `bytes_and_requests_are_separate_buckets`, `an_overrun_is_still_owed_on_the_next_operation` | one bucket charged twice, so a byte-heavy tenant exhausts its request quota; and a balance that saturates at zero, which forgives every overrun and bounds nothing |
| 7 | `a_short_read_bills_what_it_returned`, `a_refused_request_bills_nothing` | billing the requested range rather than the response — which overcharges exactly the short-read case `Broken`'s `ShortReadsPastTheEnd` exists for |
| 8 | `the_meter_and_the_accountant_agree` | a meter that counts a different set of operations than `Accounted` does |
| 9 | `get_tag_is_counted_and_never_refused` | a refusal returning `None`, which reads as "the object is absent" on the CAS rebase and turns into a create-if-absent against an object that exists |

⚠️ Criterion 3 is the one that makes criterion 2 insufficient. A meter that reserves inside the
fan-out loop refuses *something* and passes every "is it refused" test, while having already
issued the requests the quota existed to prevent.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Any operation, inside quota | **unchanged** | unchanged | unchanged | 0 |
| Any operation, over quota | **0** | **0** | 0 | 0 |
| Reading a tenant's usage | 0 | 0 — in memory | 0 | 0 |
| A billing rollup | **not built**; it is one enumeration, which M6a already prices | — | — | 0 |

## Risks

- **A per-node meter is not a per-tenant quota.** Nodes own nothing and there is no
  coordination, so a tenant's true rate is its per-node rate times the nodes it reaches. This
  meter bounds the damage one node does, which is what a token bucket in a shared process can
  honestly claim; a fleet-wide quota needs a number no node holds. **Named in the module docs,
  not just here** — it is the kind of limit a reader will otherwise assume away.
- **Refusing is a decision with a blast radius.** A quota that fires during a fold refuses a
  write the caller may have been told is durable. This milestone puts the meter on the trait
  and does not wire it into any stack, so nothing is refused yet — which is deliberate, and it
  means the policy question is answered when there is a server to answer it for.
- **A per-node meter is also a per-node *accountant*.** `Meter::usage` reports what this node
  saw. Summing across nodes is the billing rollup's problem and it is not built — and the
  catalog cannot hold the number, for the reason in "Does not add".
- **The byte bucket admits one overrun.** By construction: it checks the balance before and
  debits the actual after, so a single operation can exceed the quota by whatever it
  transferred. Bounded by that operation's size, never unbounded, and the alternative is a
  `head` on every read.

## Tasks

| Id | Commit |
|---|---|
| **M6b.1** | `pstore-meter`: the bucket, the clock it is told rather than reads, and the burst that bounds it |
| **M6b.2** | `Metered`, and the reservation taken for a whole fan-out before any of it is issued |
| **M6b.3** | `Meter::usage`, and the equality with `Accounted` that stops the two disagreeing |
