# M48 — A mixed-version fleet still reconciles past 112 members

**Serves:** [BACKLOG](../BACKLOG.md) row 53's residue, "a rolling upgrade past 112 members learns
old nodes' state by piggyback alone". The project owner chose this residue on 2026-10-04 over
closing the row as deferred.

## What is true today

- Above 112 members (`TAG_ABOVE`), a checksum mismatch sends a `TaggedDigest`, tag 7 (M43). A
  node built before M43 decodes tag 7 to nothing: no reply, and no liveness credit.
- An old node past 32 members sends M34's `Digest`, and a new node answers it with `Part`s. So
  an old node learns a new node's state.
- **The reverse never happens.** A new node only ever sends that old peer a `TaggedDigest`,
  which is dropped, so it learns the old node's state only by piggyback.
  - Piggyback carries recent changes, each at most `RETRANSMITS` times.
  - So a record only old nodes still hold, past its retransmissions, never reaches the new
    ones. Their checksums disagree for as long as the fleet is mixed.
- **A current node does not answer every `TaggedDigest`** (spec review). It compares the digest
  with its state on arrival, not when the sender saw the mismatch. On the ping path, the `Ack`
  carries piggybacked updates ahead of the digest. When those level the two views, every bucket
  agrees, nothing is sent, and the sender hears silence from a current peer. That is the
  ordinary case during churn.

## Delta

**A `TaggedDigest` is always answered, and a node falls back to M34's `Digest` for a peer that
has left three unanswered, going back to tags when that peer shows it understands them.**

0. **Always answered.** A `TaggedDigest` whose sums all agree with the answerer's is answered
   with one empty `Part`: 21 bytes. So silence means an old build, or loss of the digest or of
   its answer. That is about 19% per exchange at 10% loss, and about 0.7% for three in a row.
   A build from M43 to M47 does not answer the agreeing case, so it can be falsely marked. That
   is harmless (rule 4): those builds send `TaggedDigest`s above 112 themselves, which clears
   the mark (rule 3).

1. `reconcile` takes the peer it answers (both call sites hold `from`). Counting applies only
   when the view is above 112: the `Digest` and `Sync` paths at or below it are untouched.
   `Protocol` keeps, per peer, the number of `TaggedDigest`s sent to it since its last `Part`,
   and a set of peers marked **untagged**.
   - Sending a `TaggedDigest` to a peer increments its count.
   - A `Part` from the peer clears the count (removes the entry). Several split `Part`s
     clear it several times, which is harmless.
   - When a mismatch would send the peer its **fourth** unanswered `TaggedDigest`, the peer is
     marked untagged and sent a `Digest` instead.
2. **An untagged peer is sent a `Digest`** on every mismatch, which every build since M34
   answers.
3. **A `TaggedDigest` received from a peer clears its mark and its count**, because only a
   build that understands tags sends one.
4. ⚠️ **Bounded state:** both maps are keyed by member id. `learn` admits any sender as a
   member, and members are never removed, so both are bounded by the view.
   - A falsely marked peer costs M34's price for that peer (correct, and a little dearer)
     until it sends a `TaggedDigest`.
   - A peer whose own view stays at 112 or below never sends one, so it stays marked. That
     is said, and it is accepted.
5. **Not changed:** the wire format; how a differing digest is answered; M43's thresholds;
   M44's splitting; and every existing test's bound. The `Sim` already counts `Digest` and
   `Part` as reconciliation, so the "goes quiet" tests keep their meaning. `pstore-node`
   needs no change.

## Acceptance criteria

1. **An agreeing digest is answered.** A node answering a `TaggedDigest` equal to its own
   sends exactly one `Part`, with no members. Test:
   `protocol::tests::an_agreeing_tagged_digest_is_answered_empty`.
2. **Silence falls back; tags return.** A node viewing 121 members, receiving mismatched pings
   from one peer, sends that peer:
   - a `TaggedDigest` three times;
   - a `Digest` on the fourth mismatch;
   - still a `Digest` after a `Part` from it;
   - a `TaggedDigest` again after receiving one from it.

   Test: `pstore-gossip` `protocol::tests::a_peer_that_never_answers_tags_is_sent_a_digest`.
3. **An answering peer never falls back.** Ten mismatches, each followed by a `Part` from the
   peer, send ten `TaggedDigest`s. Test: `protocol::tests::an_answering_peer_keeps_its_tags`.
4. **A mixed pair converges.**
   - A new node and an emulated old one, each viewing 121 members, **119 of them Dead in both
     views**, so each node's only live peer is the other. Every probe pairs them, except the probe
     of a Dead member every `REVISIT_DEAD_EVERY` (5) ticks. The old one drops every
     `TaggedDigest` delivered to it, and sends `Digest` where the new code sends
     `TaggedDigest`.
   - The old node holds a newer record of one member than the new node, with nothing left to
     piggyback.
   - Driven by each node's own `tick`, delivering every message between the two, within 6
     ticks the new node holds the old node's record, and the two checksums
     agree.
   - With the fallback disabled (hand mutation), they do not converge in 6 ticks. That shows
     the test needs the fallback.
   - Test: `protocol::tests::a_mixed_version_pair_converges`.
5. **Nothing else moves, measured.** Every existing gossip and node test passes unchanged,
   `./scripts/gates.sh`. That includes `a_tagged_digest_cuts_heavy_loss_at_200` (≤ 1,300 bytes
   per node per round; 1,043 is M43's figure) and M33's suspicion bounds.
   The ledger records the figure measured on this milestone's parent and on its code, side by
   side.
6. **Mutation:** the incremental sweep of the changed lines misses 0
   (`--no-config --profile mutants`, as M34's and M43's were). `./scripts/mutants.sh`.
