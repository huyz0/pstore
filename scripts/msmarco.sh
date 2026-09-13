#!/usr/bin/env bash
# M5's exit criterion: BM25 on MS MARCO passage ranking.
#
# ⚠️ **Not a gate, and not in `gates.sh`.** It needs a 1 GB download and minutes of indexing —
# the same argument that keeps `recall.sh` and `depth.sh` out of `cargo test`, one size up. The
# roadmap wants NDCG/MRR as CI gates; that needs the corpus present in CI, which is a different
# problem from measuring it once.
#
# ⚠️ The corpus is **fetched, never vendored**: 1.06 GB compressed, 3.1 GB of passages.
#
#   scripts/msmarco.sh                     the full dev set, 6,980 queries
#   PSTORE_MSMARCO_QUERIES=200 scripts/msmarco.sh   a sample, for checking the harness
set -euo pipefail
cd "$(dirname "$0")/.."

DIR=${PSTORE_MSMARCO_DIR:-$HOME/.cache/pstore-msmarco}
URL=https://msmarco.z22.web.core.windows.net/msmarcoranking/collectionandqueries.tar.gz

if [[ ! -f "$DIR/collection.tsv" ]]; then
    echo "fetching MS MARCO into $DIR (1.06 GB)" >&2
    mkdir -p "$DIR"
    curl -sL --retry 3 -o "$DIR/collectionandqueries.tar.gz" "$URL"
    tar xzf "$DIR/collectionandqueries.tar.gz" -C "$DIR"
fi

for f in collection.tsv queries.dev.small.tsv qrels.dev.small.tsv; do
    [[ -f "$DIR/$f" ]] || { echo "missing $DIR/$f" >&2; exit 1; }
done

# ⚠️ `--release`. A debug build of this indexes 8.8M passages at a rate that makes the run
# look broken rather than slow.
PSTORE_MSMARCO_DIR="$DIR" exec cargo run --release -q -p pstore-query --example msmarco
