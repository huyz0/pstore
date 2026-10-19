# M105 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, against `MemoryStore`. Nothing here is a
measurement: OQ-6 is a question about behaviour, and a fake answers it only for the tag forms it
models.

1. **Multipart tags.** `a_multipart_tag_counts_its_parts_and_follows_the_content`
   (`cargo test -p pstore-blob --test memory_tags`): 10 bytes in parts of 4 → `-3`, 4 → `-1`,
   5 → `-2`, empty → `-1`. Equal content gives an equal tag, 256 bodies give 256 tags, a change
   in the last part moves the tag, and the stale tag after `v1`, `v2`, `v1` lands. Red first
   against the stub that returned a monotonic tag: `got 1`.
2. **Opaque tags.** `an_opaque_tag_is_fresh_per_write_and_not_a_number`: the same body
   written three times, once through a clone, gets three tags, and 256 more writes get 256
   distinct tags; none parses as an
   integer; the stale tag is refused. Red first against the stub that returned a content tag:
   `equal content written twice got one tag`.
3. **Conformance.** `the_suite_tells_a_multipart_tag_from_an_opaque_one`
   (`cargo test -p pstore-testkit --test conformance`): `Multipart { part_bytes: 4 }` diverges on
   `aba_resistance` alone, and `Opaque` conforms.
4. **The HEAD guard.** `a_head_that_does_not_advance_the_epoch_is_refused`
   (`cargo test -p pstore-engine --test oq6`): an equal and a lower epoch are each `Refused`, and
   HEAD is unchanged. Red first, each case on its own: `Ok(Epoch(2))` for the equal epoch,
   `Ok(Epoch(1))` for the lower. `commit_stale_for_test` gives `Lost` in
   `a_stale_committer_is_fenced` (`--test engine`) and `a_writer_paused_past_many_commits_cannot_corrupt`,
   and `BackendCannotFence` in `a_commit_reached_past_the_doors_is_refused` (`--test divergent`).
5. **The answer, on the engine.** `no_two_heads_share_bytes_or_a_tag_under_any_tag_style`:
   under `ContentHash`, `Multipart { part_bytes: 64 }` and `Opaque`, at least 9 HEADs each, including
   the drop, the same index recreated with the same documents, GC and the change-nothing
   commit, and no HEAD's bytes or tag repeat. Red first against the mutant the spec names, with
   the guard disabled and `commit_head_for_test`'s epoch line deleted: `ContentHash: a HEAD's
   bytes repeated`, 8 distinct bodies for 9 HEADs, and the store accepted the repeat.
6. **A stale tag under every style.** `a_held_tag_is_refused_and_the_retry_keeps_the_other_write`:
   under each of the three styles, the compaction's and the drop's held tags each lose a CAS,
   the retry lands, `new` and `o` survive, and `idx` is gone.
   `one_hundred_writers_lose_nothing_under_any_tag_style`: 100 of 100 documents under each
   style, and no HEAD's bytes repeat. Neither was seen red: they answer a question, and the
   control that they can fail is criterion 5's.
7. **OQ-6** is struck through in `open-questions.md`, linking here, with the adapter's ETag round
   trip left open. `manifest-and-cas.md` §5 carries the correction banner.
   `grep -c "Corrected by \[M105\]" docs/research/03-metadata-consistency/manifest-and-cas.md` → 1.
8. **Gates.** `./scripts/mutants.sh` over the diff, 9 mutants: 7 caught, 1 unviable
   (the tag function returning a default tag), 0 missed, and 1 timeout: the HEAD commit mutated to return `Ok` unwritten,
   which claims a commit it never wrote, so a caller waiting to see its write never stops.
   ⚠️ The sweep ran in two parts. The first reached the background time limit after 5 mutants;
   the other 4 ran under `--check` scoped to `pstore-blob`, `pstore-engine` and
   `pstore-testkit`. `./scripts/gates.sh`, by the pre-commit hook on this commit.
