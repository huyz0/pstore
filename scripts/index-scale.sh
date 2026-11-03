#!/usr/bin/env bash
# Thousands of active indexes in one tenant, on one server (M169): at 50, 1,000 and 5,000
# indexes, every index written, folded and queried, then one written and folded among them, a
# fresh server's query, and the per-tenant limit at the count.
#
# ⚠️ Outside `cargo test`, for the reason `depth.sh` gives: 5,000 indexes is minutes in a debug
# build, and `cargo mutants` reruns the suite once per mutant. The suite keeps the same
# assertions at 2 and 40 indexes (`index_scale::request_counts_do_not_grow_with_indexes`).
#
# ⚠️ Refuses unless the test passed: a renamed test would otherwise match nothing, report
# "0 passed", and leave this gate green while checking nothing (gossip-loss.sh's lesson).
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(cargo test --release --quiet -p pstore-server --test it -- --ignored \
    --exact index_scale::thousands_of_indexes_in_one_tenant --nocapture "$@" 2>&1) \
    || { printf '%s\n' "$out"; exit 1; }
printf '%s\n' "$out"
case "$out" in
    *"test result: ok. 1 passed"*) ;;
    *) echo "index-scale.sh: the index-scale test did not run" >&2; exit 1 ;;
esac
