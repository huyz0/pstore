# M72 — A configurable probe count for the clustered `fde` leg

**Serves:** backlog row 77, the part buildable without an owner decision. [M71](../M71/SPEC.md)
read "a third of the lists, at least two" with no way to say otherwise.

## Delta

- `Prefetch::MultiFde` gains `probes: Option<usize>`: the lists of the clustered encoding to read
  in a segment that has them. `None` keeps M71's default. A segment without clusters (the flat
  scan) ignores it.
- The API's `multi` object accepts `probes`, a whole number from 1 to 1024, **only with
  `mode: "fde"`**; anything else is `400` naming `probes`.

## Acceptance criteria

1. `fde_options_are_refused_at_the_door` (server): `probes` with the exact mode, `0`, `1025`, a
   float, a string and a negative are each `400` naming `probes`; `1` and `1024` are served.
2. `more_probes_read_more_and_all_of_them_read_everything` (engine): over a flagged 9,000-document
   segment, `probes: 1` moves fewer bytes of `.fdev` than the default, which moves fewer than
   `probes` at every list; the answers' top-10 mean score at all lists is at least the default's.
3. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0, or
   each miss is killed by a named test.

**Not covered:** the rest of row 77 (a second multi-vector field per segment, the in-memory fresh
segment) and rows 73-76 and 78.
