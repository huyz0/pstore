# M72 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store; bytes are provisional.

1. `fde_options_are_refused_at_the_door` (`cargo test -p pstore-server --test multi_fde`): `probes`
   of 0, 1025, 2.5, `"3"` and -1 are `400` naming `probes`; `probes` beside the exact mode is
   `400`; 1 and 1024 are served.
2. `more_probes_read_more_and_all_of_them_read_everything` (`cargo test -p pstore-engine --test
   fde_skip`): over a flagged 9,000-document segment, `.fdev` bytes moved are **4,608,000** at
   `probes: 1`, **9,216,000** by default (two lists) and **18,432,000** at every list; the top-10
   mean score does not fall as probes rise.
3. **Gates:** green by the pre-commit hook on this commit. **Mutation** (`cargo mutants`,
   `--no-config`, `pstore-server`, test selection the server API suite): the filter also pulled in
   19 mutants in all; **the 4 in the probe-count validator were all caught**. The other 15 missed are struct
   fields of S3/Azure capability tables, the retry hint and `exact` in unrelated functions, which
   that suite does not exercise and the workspace suite does; ⚠️ not re-run under the
   whole-workspace config.

**Not covered:** the rest of backlog row 77 (a second multi-vector field per segment, the
in-memory fresh segment) and rows 73-76 and 78.
