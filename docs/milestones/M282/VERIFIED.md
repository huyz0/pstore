# M282 verification

The spec was not sent to a separate reviewer before code; the PR's review covers it.

1. `a_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated` passes. Run first
   with `s3_store` as moved from `main.rs`, unchanged, it failed with `Err(Lost)`: the
   client repeated the PUT and the fake answered the repeat `412`.
2. `an_azure_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated` passes. With
   `azure_store`'s conditional client removed it failed the same way, `Err(Lost)`.
3. `a_read_that_gets_a_503_is_retried` and `an_unconditional_write_that_gets_a_503_is_retried`
   pass, before the change and after: two GETs, two PUTs.
4. `the_retry_policies_are_pinned` passes.
5. `a_registration_that_lands_and_then_errs_keeps_its_codes` passes. Before `bury_into` used
   `head_keys` it failed with `live codes buried: [".../ext/x/...codes"]`.

Also: `cargo test -p pstore-engine` (69 unit, 664 integration) and `cargo test -p pstore-server
--test it` (738) pass. The server's two IPv6 unit tests fail in this container with or
without the change: it has no IPv6 loopback.
