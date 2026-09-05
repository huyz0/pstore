# pstore

A masterless, object-storage-native search engine in Rust — vector ANN + BM25 + filtered
hybrid search — where the blob store is the **only** durable tier, for data *and* metadata.

| | |
|---|---|
| Unit of tenancy | **index** (millions of them) |
| Durable state | S3 / GCS / Azure Blob only. No Postgres, etcd, ZooKeeper, or DynamoDB. |
| Topology | No master, no leader election. Target: **10,000 nodes**. |
| Local state | RAM + NVMe are pure cache. Nodes own nothing. |
| Blob API discipline | LIST banned on hot paths; writes batched; reads free. |

**Status: research phase complete, no code yet.**
Start at **[docs/research/INDEX.md](docs/research/INDEX.md)**, then
[the architecture](docs/research/11-design/architecture.md) and
[the roadmap](docs/research/11-design/roadmap.md).
