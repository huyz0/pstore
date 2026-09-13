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

**Status: building.** 13 crates, ~44k lines of Rust, 722 tests, 30 milestone ledgers.
M0a, M1–M5 and M6 are through their exit criteria — with M6's recorded as **partly met**,
because one half of it was measured and failed. What is done, what is not, and what is
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
