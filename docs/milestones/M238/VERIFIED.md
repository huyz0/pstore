# M238 verification

Tests: `cargo test -p pstore-server --test it ingress` (15: M128's nine unchanged and green
with the setting at its default, off; six new). The whole `pstore-server` suite green but for
the two `[::1]` tests this container cannot run. Smoke: `./scripts/tls-smoke.sh`.

Red runs: the new tests name `RequestLimitsBuilder::drain_refused_bodies`, which the parent
lacks, so they were red by not compiling; the first mutation below (no drain) is the parent's
behaviour, and fails three of them. Mutations, one at a time by a script and reverted: 7 of 7
killed. No drain (criteria 1, 3, 4); drain only on 401 (1); the declared-length check dropped
(2); no byte bound (3); no deadline (4); default on (5, and M128's
`authorization_precedes_body_polling_and_admission`, `ingress_byte_limits_precede_engine_access`
and `unsupported_encoding_never_reaches_json`, which pin off); the config field ignored (5).
No `cargo mutants` run is claimed.

1. `a_refused_body_is_read_to_its_end_when_draining`: green. No credential, a malformed one
   (401), a read-only one (403, with and without an encoding), 415 and 429: each read all
   1,000 bytes, kept its status and code, left admission at zero and the store unrequested.
2. `a_body_declared_too_large_is_never_read`: green, the spy never polled at 1,025 > 1,024.
   `a_body_behind_expect_continue_is_never_read` (added in code review): green, 0 bytes read;
   red with the `Expect` check removed.
3. `draining_stops_at_its_bound`: green, 4,096 bytes offered in 128-byte chunks, exactly 1,024
   read: the bound reached exactly, not one chunk past it.
4. `a_stalled_refused_body_is_abandoned_at_the_deadline`: green, the 401 sent at exactly the
   500 ms deadline (paused clock).
5. `drain_refused_bodies_is_read_from_the_config`: green; the M128 suite green, off.
6. `./scripts/tls-smoke.sh` green 3 runs in a row with it on, its `a 1536 KiB body with no
   credential is pstore's 401` and `a 2048 KiB body ...` lines included. Measured with a copy of
   the script, 20 unauthenticated writes per size, `502`s off then on (`provisional`, one
   container, nginx 1.24):

   | proxy | 1 MiB | 1.5 MiB | 2 MiB |
   |---|---|---|---|
   | buffered (the documented file) | 0, 0 | 0, 0 | 7, 0 |
   | streamed | 17, 0 | 17, 0 | 20, 0 |

   The default stays **off**: the only workload where on wins is a server behind a proxy
   with refused bodies over about 1 MiB; everywhere else on costs, per refused request, its
   body's bytes read and dropped (up to the body bound, by construction in criterion 1) and the
   connection held up to the read deadline (criterion 4), at the call of a peer that may hold
   no credential. docs/deploy.md tells an operator behind a proxy to turn it on.

Code review, one round (a reviewer given the diff, not this ledger): two majors. A client
sending `Expect: 100-continue` would have been asked to upload a body only to be refused:
fixed, with a test. `nginx-light` might not exist on Ubuntu 24.04: it does (installed here,
1.24.0-2ubuntu7.19), kept. Minors fixed: the smoke script's address probe on a host with no
route; the docs say a drain is bounded per request and not in number, and that a `413` from
collection drains up to `body_bytes` more. Minors left: `free_port` can race another process
for a port (nginx then fails to start, loudly).

CI's mutation shard found `n <= max` for `n < max` in `discard` missed: at 100-byte chunks both
read 1,100 bytes, and review had left it as equivalent. It is not: a body that reaches the bound
exactly is read one chunk further. Criterion 3's test now offers 128-byte chunks and asserts
exactly 1,024 bytes read (it was a range); the mutant applied by hand fails it.
