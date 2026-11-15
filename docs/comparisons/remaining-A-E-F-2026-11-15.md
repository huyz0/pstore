# What is left of A, E and F, and what is built (2026-11-15)

Audit of `origin/main` at `1be38fb` against [DELIVERY.md](DELIVERY.md) and the
[delivery plan](turbopuffer-next-plan.md). Since the 2026-11-09 audit, M203 (scoring
expressions), M204 (twelve stopword languages), M210 and M211 (named `ForEachUnique` groups,
`Min`/`Max`/`Avg`, buckets), M212 (a plan's candidate rows reserved) and M213 (buffered rows
charged their memory) merged. This note decides every item still open in A, E, F and the
shared C/F row. API-shape gaps belong to the separate API comparison and are only listed.

| Row | Item | Decision | Why |
|---|---|---|---|
| C/F | Lane tails probed per leg; legs at mixed levels or `as_of` run apart | **Build: [M236](../milestones/M236/SPEC.md)** | The architecture proof holds (in the spec): a stronger snapshot keeps a weaker level's promise, one probe answers for every strong leg, and an `as_of` leg reads the same history from any HEAD read now. Fewer requests for every shape and the same depth, so no setting. It closes the plan's "all subqueries bind one snapshot" for every request shape. |
| A | Reproducible TLS and gateway smoke test (the plan's risk note) | **Build: [M237](../milestones/M237/SPEC.md)** | The docs say to terminate public TLS in a reverse proxy, and nothing runs that configuration. A script that starts the real server behind a real TLS proxy, and checks identity is the signed token's and never a forwarded header, turns the claim into a gate. No product behaviour changes. |
| A | 401/403/413/429 behind a proxy | **Built: [M238](../milestones/M238/SPEC.md)**, a setting, off by default | Found by M237: pstore refuses some requests before it reads their body and closes; nginx then answers `502`, up to 20 of 20 tries at 2 MiB. Draining a refused body fixes it (0 of 20) but lets a peer with no credential make the server read and hold, so it is opt-in for servers behind a proxy. |
| A | Bound compressed bytes | Nothing to build | The server accepts no `Content-Encoding` (M128 refuses it `415`), so there are no compressed bytes to bound. Revisit if compression is ever accepted. |
| A | 512 MB writes as a product promise | Not offered | The plan makes it a promise only after decode-memory, concurrency and throughput measurements at that size; the configured body limit stays the contract. |
| E | Stopwords for Arabic, Greek, Romanian, Turkish, Tamil | **Not built** (stays refused, by name) | M204 took its lists from Snowball as tantivy ships them; Snowball has none for these five, and the vendor documents no list of its own. Lucene has Apache-2.0 lists for four, but adopting a second source would pick a stopword set the vendor may not share, which is the "inferred default" the plan forbids, and Tamil has no list anywhere. A refusal is honest; revisit when the vendor publishes its lists. |
| E | `rank_fragments_by` | Left to the API comparison | Refused because the vendor publishes no shape for its value; an API-contract question, not engine work. |
| E | Computed outputs (`VectorDist`, computed BM25 scores as attributes) | Left to the API comparison | A response compute map is an API surface. The scores themselves exist (M182, M203). |
| E | `k3` query-term tuning | Not offered | The vendor's wire contract is unresolved (comparison row "BM25 query-term tuning"). |
| E | WAND / upper-bound pruning for expressions | Not now | The plan orders it after the exhaustive oracle is stable and only for measured cost; no measurement asks for it yet. |
| F | Nested groups (a group's sub-groups) | **Not built** | The vendor has none (`group_by` is up to 8 flat keys, which pstore serves); only Vespa's grouping language has them. The response grows multiplicatively and needs its own limits, for no parity gain. Composite keys answer the same counts flatly. Record as not offered. |
| F | Model reranking | Out of scope (J) | Needs a model provider. |
