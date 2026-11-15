# M237: public TLS and the gateway, run as documented

## Serves
Workstream A of [the delivery plan](../../comparisons/turbopuffer-next-plan.md), its risk note:
"Deployment TLS and gateway configuration need a reproducible smoke test; parser tests do not
prove them." [DELIVERY.md](../../comparisons/DELIVERY.md)'s A admission row. No D- or OQ- id.

## Delta
1. **`deploy/nginx/pstore-tls.conf`**: the reverse proxy [deploy.md](../../deploy.md)'s `tls`
   section asks for, as a file: TLS 1.2 and 1.3 only, the `Authorization` header passed through
   untouched, no identity of its own, a body bounded at pstore's own `body_bytes` (2 MiB) and
   buffered whole before it is forwarded, and an oversized body answered with pstore's own
   typed `413`, byte for byte.
2. **`scripts/tls-smoke.sh`** (`portable: no`): a throwaway CA and server certificate, the real
   `pstore-server` binary with signed-credential authentication (M127) on the in-memory store,
   bound to loopback, and nginx running that file as written (only `@NAME@` placeholders
   filled). It checks from a client's side, over HTTPS, each criterion below.
3. **CI** runs it in a job of its own, `tls-smoke`, with `nginx-light` installed.

Found by it, before the proxy settings in 1: pstore refuses some requests before it reads
their body (M128: a bad credential, admission, an oversized `Content-Length`) and closes. A
proxy still writing that body is reset and answers `502`, not pstore's refusal. Streamed, the
proxy turned a 1 MiB unauthenticated write's `401` into `502` in 2 of 5 tries, 1.5 MiB in 5 of
5 (`provisional`, one container). Buffering and the 2 MiB bound make it rarer, not impossible:
see Risks. Not changed here: the server, any limit, any error.

## Acceptance criteria
1. A signed client creates an index, writes a row durably and reads it with a `strong` query,
   over HTTPS through the proxy; a tenant header equal to the credential's is accepted.
2. Through the proxy: no credential `401`; an expired one `401`; a `read`-only one writing `403`.
3. A valid credential with a forged tenant header is `403`, and the other tenant lists no index.
4. A tenant credential on an admin route is `403`.
5. A body of exactly 2 MiB reaches pstore and is answered; one byte more, and 3 MiB, get the
   typed `413` pstore itself sends for that body; a 256 KiB unauthenticated body gets `401`.
6. Plain HTTP on the TLS port is refused by the proxy and never answered by pstore.
7. pstore answers on no address but loopback (`NOT-RUN` on a host with no other address).
8. The script passes 5 runs in a row here, and in CI.

## Test plan
The script's own checks, named in its output. Mutations of the configuration and script,
each expected to fail a check: the proxy's `error_page` removed (5: an HTML `413`);
`client_max_body_size 4m` (5: the 3 MiB body `502`); streaming instead of buffering (5, the
early refusal, at a size where it is observed); pstore bound to `0.0.0.0` (7).

## RA budget
Unchanged: no server code changes.

## Risks
- The `502` remains possible for a body pstore refuses unread, below 2 MiB, once it outgrows the
  kernel's socket buffers: a proxy that has to send it into a closed connection. The fix is
  server-side (read and discard a refused body, bounded), which M128's "authorization precedes
  body polling" forbids today; it is a setting of its own (M238).
- Kernel buffer sizes differ between hosts, so check 5's 256 KiB is chosen well under the
  1 MiB measured here.

## Tasks
- M237.1 spec, config, script. M237.2 CI job, Gates row, deploy.md. M237.3 VERIFIED, DELIVERY.
