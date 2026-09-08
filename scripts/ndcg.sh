#!/usr/bin/env bash
# The ranking-quality gate (D-31).
#
# ⚠️ Runs OUTSIDE `cargo test`, for the reason `recall.sh` does: a judged corpus inside the
# unit suite is rebuilt once per mutant by `cargo mutants`, which runs the suite hundreds of
# times.
#
# ⚠️ **It scores two rankers, not one.** A judged set we generated is ranked correctly by any
# term-matching scorer, so the gate also scores a control with IDF removed and FAILS if the
# control clears the floor — otherwise the number is a measurement of the corpus, not of the
# ranker. `gate-design`: a check that cannot fail reports success while checking nothing.
#
# ⚠️ `provisional`: a synthetic corpus with planted relevance on WSL2. The roadmap's MS MARCO
# evaluation is NOT-RUN, blocked on data rather than on effort.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --release --quiet -p pstore-index --example ndcg -- "$@"
