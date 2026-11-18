# M282: never repeat a conditional write that got a status

## Serves
D-99 (CAS is the fencing mechanism) and M17 (a lost acknowledgement is not a second writer),
through lesson 6 of [lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md).

## Delta
1. **Conditional writes go through a client with no retries.** `ObjectStoreBackend` gains
   `with_conditional(client)`; `s3_store` and `azure_store` build the bucket twice, once with
   `retries()` for everything else and once with `conditional_retries()` (none) for
   `put_conditional`. `object_store` 0.14 retries any request, a conditional PUT included, on
   a `5xx`, `429` or `408`: a write that landed and got a `503` was repeated, failed its own
   precondition, and came back `Lost`. On a bundle that is "another writer took the lane";
   on a HEAD commit, a lost race whose keys the caller then abandons.
2. **The other retries are pinned, not inherited**: `retries()` writes down `object_store`'s
   0.14 defaults (10 retries, 100 ms to 15 s backoff, 3 minutes), so a dependency bump
   cannot change them unseen.
3. **`s3_store` moved from `main.rs` to the library**, so the policy is reachable from a test.
4. **`bury_into` uses M281's `head_keys`.** Its own copy of the live set lacked an external
   index's codes, so a registration whose HEAD commit landed and then answered an error
   buried the codes HEAD named (M281 keeps GC from deleting them; this keeps them out of
   the graveyard).

**No setting.** The repeat cannot be made safe: the client cannot know whether the first
attempt landed, and the only answer it can give after a repeat is wrong in the one case that
matters. The cost, a `5xx` on a conditional write surfacing as an I/O error instead of being
retried inside the client, falls on paths that already handle it: a bundle is read back
(M17), a fold runs again next tick, a request fails with a `5xx` its client retries.

## Acceptance criteria
1. Against a fake S3 that stores a conditional PUT and answers `503`, `put_conditional`
   answers `Io`, after exactly one PUT:
   `a_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated`.
2. The same against Azure's client:
   `an_azure_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated`.
3. A read and an unconditional write that get a `503` are retried and succeed:
   `a_read_that_gets_a_503_is_retried`, `an_unconditional_write_that_gets_a_503_is_retried`.
4. The policies' numbers: `the_retry_policies_are_pinned`.
5. A registration whose HEAD commit lands and then errs leaves its codes unburied and GC
   keeps them: `a_registration_that_lands_and_then_errs_keeps_its_codes`.

## Test plan
Criteria 1, 2 and 5 are written first and must fail on the code before the change.
