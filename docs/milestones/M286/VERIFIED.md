# M286 verification

The spec was not sent to a separate reviewer before code. The PR's review covers it.

1. Conformance through both clients on the fake S3, the same probes and the same observed
   profile, and no signature refused: `native_s3_conformance`, with its control
   `a_bad_secret_is_refused_by_the_fake`. Command:
   `cargo test -p pstore-blob --features object_store --test it native_s3`, 11 passed.
   Mutation killed: `get_range` with its short-read check removed (`if false`) failed this
   test and `reads_answer_exactly_the_bytes_asked_for_or_an_error`.
2. SigV4 against AWS's `get-vanilla` and the S3 ranged GET Object example:
   `sigv4_matches_the_aws_examples`. Command:
   `cargo test -p pstore-blob --features s3-native --lib s3`, 11 passed. Mutations killed:
   unsorted signed headers, and an unsorted canonical query
   (`keys_and_queries_are_encoded_the_way_s3_canonicalises_them`).
3. `a_503_is_slowdown_after_one_request` (two faults, two requests),
   `a_409_is_contended` (one request), and
   `a_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated` (one PUT). Same
   command as 1. Mutation killed: the 409 arm removed fails `a_409_is_contended`. Found in
   self-review and added: `a_stalled_server_times_out` (no request timeout existed) and
   `a_server_that_ignores_range_still_answers_the_range` (a `200` answer to a suffix read
   returned the whole object; the mutation restoring that fails it). No retry
   loop exists to remove; the request counts are what would catch one being added.
4. `cargo run --release -p pstore-blob --features object_store,s3-native --example s3_clients`
   with `PSTORE_BENCH_SCALE=10`, on 2026-11-18 in a 4-core cloud container, `provisional`.
   This was the final run, after the request timeout was added. Bytes allocated per request
   were 25% or more lower on 5 of 7 patterns (WAL put −56%, fan-out −55%, segment put −55%,
   HEAD −55%, delete −85%), and CPU was 25% or more lower on 2 (fan-out −41%, HEAD −30%).
   The worst CPU result was the segment PUT at +6%, within the column's measured run-to-run
   noise of up to 15%. The rule holds. The table is in
   `docs/comparisons/object-store-layer-2026-11-18.md`.
5. `cargo tree -p pstore-blob --no-default-features --features s3-native -e normal` has
   58 crates, and none of them is `chrono`, `humantime`, `reqwest` or `quinn`. With
   `object_store,object_store/aws` it has 132. NOT-RUN as a test: recorded from the
   command's output.
