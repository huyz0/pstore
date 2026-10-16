# M94 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. Every number below is
`provisional` and relative; no latency is claimed.

Commands:
- `cargo test -p pstore-engine --test split`: 12 tests.
- `cargo test -p pstore-server`: every suite green, 58 `test result: ok` lines.
- `cargo test -p pstore-query --test splittable`: 1 test.

1. **A split `multi` query equals the unsplit one.** `a_split_multi_query_equals_the_unsplit_one`:
   six leg sets (`multi`; `multi_fde` with no rerank, rerank 20, rerank 20 with 2 probes; each of
   `multi` and `multi_fde` beside a dense leg) over four folded segments on three servers: hits,
   scores in bits, ids, attributes and distances equal the unsplit answer, and shares were served.
   - **Seen red first:** "was not split" on every leg set (`shares` returned no share).
2. **A text leg beside a multi leg splits in two phases.**
   `a_split_multi_with_text_equals_the_unsplit_one`: `multi`+text and `multi_fde`+text equal the
   unsplit answer, with phase 1 and phase 2 both served.
   - **Seen red first:** "was not split in two phases".
3. **The wire is bit for bit.** `a_part_survives_the_wire_bit_for_bit` now carries two `multi_fde`
   legs (`rerank` and `probes` both `Some` and `None`), a `multi` leg with `-0.0`, `1/3` and
   `f32::MIN_POSITIVE`, and a target with `fde_clusters: true`; the round trip compares the
   `Debug` of legs and targets. `a_leg_number_is_named_once_across_every_kind` refuses a repeated or
   out-of-range leg number across the new variants. `an_older_coordinators_target_has_no_clusters`
   decodes a target without the field as `false`.
4. **Over HTTP, split equals unsplit, and the split happens.** `a_multi_query_is_split`
   (the old `a_multi_query_is_not_split`, inverted): six query shapes answer as one server alone
   does and `pstore_peer_parts_sent` rises for each.
   - **Seen red first:** "was not split" on the first shape.
   - ⚠️ M61's ledger names the old test; it now points at the new one and says so.
5. **Clustered segments split too.** `a_split_clustered_fde_query_equals_the_unsplit_one`: two
   folded segments of 4,500 rows, a `multi_fde` leg with and without `rerank`, equal to the
   unsplit answer, with the share served and the part's targets flagged `fde_clusters`.
   - ⚠️ The flag's crossing the wire is proven by criterion 3, not here: the engine harness hands
     the `Part` over without serialising it.
6. **Failures cost rounds, not answers.** `a_failed_peer_costs_rounds_not_answers_for_a_multi_leg`:
   a dead peer and a failing store; `multi` and `multi_fde` alone, beside a dense leg and beside a
   text leg: the answer is the one server's and each failed part is counted once.
   - ⚠️ Found missing by the independent review (major): the spec promised it and the first
     draft did not carry it.
7. **Which legs split in one exchange is pinned.**
   `only_legs_that_score_a_row_alone_split_in_one_exchange`.
   - **Killed by hand:** `splittable` made `true`: "Text { .. } left: true". The sweep had left
     that mutant MISSED before this test existed.
8. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on the code commit `2dcb0b4`.
   - **Mutation**, `cargo mutants --in-diff` over the diff (17 mutants), run in two parts because
     the first was killed by the harness's background time limit after 11:
     - Part 1 (11): 9 caught, 1 unviable, 1 MISSED (`splittable -> true`, since pinned by
       criterion 7).
     - Part 2: the six remaining, all in `peers.rs`, were rerun with `cargo mutants --file
       crates/pstore-server/src/peers.rs -F 'replace (floats_of|guard|decode|WirePart::of) ->'`,
       which also matches seven neighbouring mutants: 13 tested, 10 caught, 3 unviable, 0 missed.
     - ⚠️ The `splittable` kill was by hand, not by a rerun of the sweep.
   - **Independent review** (a separate agent, given the diff and no reasoning): no blocking issue,
     one major (criterion 6, fixed), minors: stale `docs/deploy.md` (fixed), no rollout note (added),
     the internal endpoint's legs unbounded as dense legs are (left), untested `multi` + filter and
     `multi` + `sum` (left; they reuse the paths the existing split tests cover).

Not changed: protocol constants, the HTTP API, depth, the number of requests per segment.
⚠️ A coordinator newer than a peer sends a leg the peer cannot decode; the share fails, is counted
in `pstore_peer_parts_failed`, and runs on the coordinator with the same answer (criterion 6).
