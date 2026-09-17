# pstore

A masterless, object-storage-native search engine in Rust — vector ANN + BM25 + filtered
hybrid search — where the blob store is the **only** durable tier, for data *and* metadata.

| | |
|---|---|
| Scale | **1M tenants × up to 50 indexes ≈ 50M indexes**; 10% of tenants active per second |
| Unit of tenancy | **index** (API/schema/query); the **tenant** is the unit of commit |
| Durable state | S3 / GCS / Azure Blob only. No Postgres, etcd, ZooKeeper, or DynamoDB. |
| Topology | No master, no leader election. Target: **10,000 nodes**. |
| Local state | RAM + NVMe are pure cache. Nodes own nothing. |
| Blob API discipline | LIST banned on hot paths; writes batched; reads free. |

**Status: building.** 14 crates, ~50k lines of Rust, 802 tests, 36 milestone ledgers.
**There is a server**: `cargo run -p pstore-server` with `PSTORE_LANE` set answers
`PUT /v1/indexes/{id}/documents` and `POST /v1/indexes/{id}/query` over HTTP, and every
response says what it cost in blob requests and how fresh it is — with no authentication and
no quota, which [M7c](docs/milestones/M7c/VERIFIED.md) states rather than implies.
⚠️ **And it runs in a container against a real object store**: `scripts/byoc.sh` brings up
**two servers on two lanes against one bucket**, and a document written and folded through one
is returned by the other. That is the first thing in this repository to cross a process
boundary; everything before it proved durability with two engines in one test binary. A server
pointed at a bucket whose capabilities have not been measured **refuses to start**
([M7g](docs/milestones/M7g/VERIFIED.md)).

M0a, M1–M5, M6 and M7 are through their exit criteria — with M6's
recorded as **partly met**, because one half of it was measured and failed, and then measured
again to decide it is not worth meeting: the byte cost it names is 0.05 ms at the shape a real
tenant has, against the 30 ms round trip that fixing it would add. What is done, what is not, and what is
blocked on a cloud account rather than on effort is in
[the roadmap](docs/research/11-design/roadmap.md) and
[the backlog](docs/milestones/BACKLOG.md).

⚠️ **Every number in this repository was measured or is marked `NOT-RUN`.** Numbers taken on
WSL2 or against an emulator say `provisional` and are relative only. Where a later
measurement overturned an earlier claim, the earlier one is corrected in place with a banner
rather than deleted — the wrong reasoning stays, because it is evidence about how we think.

Start at **[docs/research/INDEX.md](docs/research/INDEX.md)**, then
[the architecture](docs/research/11-design/architecture.md). How the work is run —
the non-negotiables, the gates, and the skills — is in [`AGENTS.md`](AGENTS.md).
Local dev environment (containerized for WSL2): [`dev/README.md`](dev/README.md).

## Licence

[Apache License 2.0](LICENSE). Copyright 2026 Huy Nguyen.
