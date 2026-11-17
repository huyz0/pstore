# What is left that needs no cloud account or model provider (2026-11-17)

Audit of `origin/main` at `bce14ef` (M272) against [DELIVERY.md](DELIVERY.md), the
[delivery plan](turbopuffer-next-plan.md), the [2026-11-15 decisions](remaining-A-E-F-2026-11-15.md)
and the facade's [manifest](../../api/compat/turbopuffer.json). Workstreams I and J, and the
cloud half of the evaluation, need cloud accounts or a model provider and are out of scope.
The owner asked for every other item to be decided and done (2026-11-17).

## Built here

| Row | Item | Milestone | Why |
|---|---|---|---|
| CI | The Windows `portable` job's `no port free for both TCP and UDP in 64 tries` (once on #69) | [M273](../milestones/M273/SPEC.md) | A test helper, not product code. Windows hands a TCP listener the port it just freed, so 64 tries can name one port whose UDP half is taken 64 times. Holding each refused listener until the search ends, and trying UDP first as well, makes every try a new port. |
| B, C | A conditional or by-filter write resent by the SDK after a lost answer may apply twice (manifest `retries`) | [M274](../milestones/M274/SPEC.md) | The plan: "Retrying after an ambiguous network failure needs a bounded, durable idempotency/result policy." The SDK sends no idempotency key but numbers its attempts (`x-stainless-retry-count`). The facade gives each first attempt a native M132 key and a resend of the same body the same key, so it applies at most once; a resend it cannot match is refused, never evaluated again. No setting: the only behaviour it removes is a silent second application. |
| B | `schema()` answers only datetime, uuid, bytes and uint64 attributes (M272) | [M275](../milestones/M275/SPEC.md) | The native summary already records the vector fields and their types, the text fields and their analyzers, and the regex attributes. Translating them costs no request. Text fields carry no `type`, because pstore does not record whether one holds strings or string arrays; the SDK reads a missing type as `None` (checked against the pinned 3.1.0). |
| Evaluation | Reproducible per-feature CPU, allocation and quality microbenchmarks | [M276](../milestones/M276/SPEC.md) | The plan's microbenchmark table needs no cloud: fixed seeds, digests of every answer, allocations per operation, warmups, repetitions and dispersion, on this host, `provisional`. Not a gate, as the plan says, until its noise is known. |

## Decided, not built

| Row | Item | Decision | Why |
|---|---|---|---|
| D | M195's later increment: f16/i8 **transport** (base64 in the declared type's bytes) | **Not built** | The pinned vendor SDK encodes every base64 vector as little-endian f32 and decodes every answer as `len / 4` f32s (`turbopuffer/lib/vector.py`), whatever the field's type. f16 bytes on the wire would break the SDK's own decoder, and the native clients have no use for a second encoding. pstore already reads f32 base64 into an f16 or i8 field, rounded or refused as an array is (M195). |
| D | M195's later increment: f16/i8 **storage** | **Not now** | It changes no answer, only bytes. At 768 dimensions a row's raw vector is 3,072 bytes as f32 and 1,536 as f16: 100M rows save 154 GB, about $3.5 a month at $0.023/GB-month. ANN queries read the quantized codes, not the raw vectors, which are fetched only for the rerank window and projection. The cost is a codec change at every place that slices a vector by `dims * 4` (about 20, across format, index and query), a readers-first rollout, and a conversion on every exact score. Revisit when a measured workload is bound by raw-vector bytes, for example exact scans of large f16 segments. |
| B | The finished async copy's `result` is a dict in the SDK, not its `Success` model (M271) | **Not built** | The SDK's `WriteResult` requires `billing`. pstore has no billing, and the plan forbids inventing the vendor's. The result is complete as a dict. |
| A | Bounded compressed bytes; 512 MB writes | As decided 2026-11-15 | No `Content-Encoding` is accepted (`415`); a 512 MB promise needs measurements at that size. Row done otherwise. |
| C | A pollable id for a `durable` (intent) write | **Not built** | `durability: "applied"` is the completion contract (M129) and answers with an operation id; an intent's pending state is the index's `progress` (M160), and no counter of an intent is labelled affected. A second, polled completion API would duplicate the applied one. Both C rows are done with this. |
| E | Stopwords for ar, el, ro, ta, tr; WAND | As decided 2026-11-15 | No list the vendor shares; no measurement asks for pruning. Both E rows are done otherwise. |
| F | Nested groups; model reranking | As decided 2026-11-15; J | The vendor has no nested groups; reranking needs a model provider. Both F rows are done otherwise. |
| C/F | The deferred shared lane tails | Already built | [M236](../milestones/M236/SPEC.md) proved and built them on 2026-11-15. |

## Blocked on the owner

Only what was out of scope already: I (cloud backend profiles, reservations, KMS,
deployment isolation), J (embedding and reranking adapters), the architecture experiment and
TCO model of the evaluation, vendor-recorded ground truth for the facade (a service
account), and the durations of M202's RPO and RTO (a real fleet).
