# M237 verification

Command: `./scripts/tls-smoke.sh` (nginx 1.24 from Ubuntu's `nginx-light`, OpenSSL 3, one
4-core container, run as root). Each criterion is a check the script prints as `ok <name>`;
the names below are those lines. Since M238 the script runs pstore with
`drain_refused_bodies` on, and its early-refusal check covers 256 KiB, 1.5 MiB and 2 MiB.

Red runs: none of these checks can be red against the parent, which had no proxy
configuration or script; they were written against the documented contract and the first
runs failed on the harness itself (a placeholder left in a comment, a CA without key usage,
which Python 3.13 refuses), then on the finding below. Mutations of the configuration and
script, each run and reverted: `error_page` removed (`one byte over gets pstore's own typed
413` failed: nginx's HTML 413); `client_max_body_size 4m` (`a 3 MiB body gets the same typed
413` failed: 502, 3 of 3 runs); streaming instead of buffering (the early-refusal check failed
in 2 of 3 runs: 502); pstore bound to `0.0.0.0` (`pstore does not answer on 192.0.2.2` failed).

Finding: streamed, a 1 MiB unauthenticated write's 401 came back 502 in 2 of 5 tries and
1.5 MiB in 5 of 5; buffered, 2 MiB in 7 of 20 (`provisional`). M238 answers it.

1. `./scripts/tls-smoke.sh`: `an index is created over HTTPS`, `a durable write over HTTPS`, `a strong query over HTTPS
   returns the row`, `a tenant header matching the credential is accepted`: ok.
2. `./scripts/tls-smoke.sh`: `no credential is 401`, `an expired credential is 401`, `a read-only credential writing is
   403`: ok.
3. `./scripts/tls-smoke.sh`: `a forged tenant header is 403`, `the forged write landed nowhere`: ok.
4. `./scripts/tls-smoke.sh`: `a tenant credential on an admin route is 403`: ok.
5. `./scripts/tls-smoke.sh`: `a body of exactly 2 MiB reaches pstore and is answered`, `one byte over gets pstore's own
   typed 413 from the proxy` (the proxy's body equal to pstore's own for that body),
   `a 3 MiB body gets the same typed 413`, `a 256 KiB body with no credential is pstore's
   401`: ok.
6. `./scripts/tls-smoke.sh`: `plain HTTP on the TLS port is refused by the proxy`: ok (400 from nginx).
7. `./scripts/tls-smoke.sh`: `pstore does not answer on 192.0.2.2`: ok, the container's one non-loopback address.
8. 5 runs in a row passed before M238, and 3 more after it with the wider check. CI's
   `tls-smoke` job: NOT-RUN here; the pull request's run is its evidence.
