# M281 verification

The spec was not sent to a separate reviewer before code; the PR's review covers it.

1. `every_kind_of_pin_keeps_a_buried_key_and_nothing_else_does` passes. Its first run, with
   `head_keys` holding only the four sources `reap_plan` checked before, failed with
   `reaped while pinned: ["an external index's codes"]`, and every other row passed.
2. `every_key_head_names_is_live` passes; it failed in the same first run.
3. `cargo test -p pstore-engine`: 69 unit and 663 integration tests pass.
