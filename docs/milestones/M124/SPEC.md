# M124: bounded lane metadata

## Serves
OQ-91 dense WAL recovery and Q9 derived-key lane discovery.

## Delta
Bound tail arithmetic before forming a probe window. Treat u64::MAX as an exhausted
tail sentinel and refuse another probe or WAL write from it. Refuse registration beyond the
existing 4096-lane decoding ceiling without changing the registry.

## Acceptance criteria
1. A missing tail near u64::MAX is returned without overflow; exhaustion refuses without probing wrapped keys or acknowledging another WAL bundle, and refused rows remain pending.
2. A full registry accepts an existing lane but refuses a new lane and remains decodable with all existing lanes.
3. Existing lane registration, request-budget, and recovery tests pass.

## Test plan
1. `tail_probing_does_not_wrap_at_sequence_exhaustion`: absent and present penultimate sequence, then exhausted sentinel. Unchecked base+window panics.
   `an_exhausted_lane_refuses_a_write_without_losing_pending_rows` resumes at the penultimate position, writes it once, then refuses the exhausted position without consuming pending rows.
2. `registering_past_the_lane_ceiling_preserves_the_registry`: seed a valid full registry, register an existing id, refuse a new id, and compare the live set. An unchecked insert makes the registry unreadable.
3. Run engine tests and fast gates.
These are deterministic invariant checks, gate-design rung 3.

## RA budget
No new requests; refusal at a full registry uses the existing registration read,
no write. Probe width remains at most 256, zero LIST.

## Risks
The exhausted sentinel cannot identify another writable sequence. Actual exhaustion
is impractical; malicious or corrupt metadata can still reach arithmetic boundaries.

## Tasks
- M124.1 Bound lane arithmetic and refuse an unrepresentable registry.
