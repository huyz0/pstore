# M127: authenticate tenant identity and authorize HTTP operations

## Serves
D-34's explicit API contract and the tenant isolation boundary.
Workstream A in [the delivery plan](../../comparisons/turbopuffer-next-plan.md).

## Delta
Add local JWT bearer verification with `jsonwebtoken` 11 (MIT, Rust >=1.88),
default features off, AWS-LC provider (already in the production dependency graph).
Pin HS256; require configured issuer/audience, key ID, expiration, decimal-string
tenant ID and explicit operation scopes. Validate optional not-before and reject
expired tokens, including the exact expiration boundary. No remote key discovery.

`PSTORE_AUTH_CONFIG` holds issuer, audience, active_kid and 1..8 unique keys
(`kid`, base64 secret of at least 32 bytes); key IDs are bounded ASCII strings.
Unknown config options, weak/invalid keys, duplicates and ambiguous modes refuse
startup without echoing secrets. Missing config also refuses standalone startup.
`PSTORE_ALLOW_UNAUTHENTICATED=1` explicitly selects the old private/test behavior;
it is incompatible with an auth config. No implicit anonymous production mode.

Bare `Api::new` / `with_cache` create closed HTTP surfaces. Add authenticated
constructors and explicitly named unsecured constructors for private embeddings and
existing fixtures. Existing tests opt into the latter without changing assertions.
Production main reads auth policy before opening cache/storage or binding a socket.

Authenticate before handlers/engine lookup; bind the tenant from verified claims.
An absent tenant header is filled; a conflicting/malformed header is 403. Scopes
are read, write, list, admin and internal. Bind permissions at route registration,
not in a second route-name inventory. Reads include queries, metadata and warming;
list is separate; admin covers admin endpoints and node-wide metrics. The previous
HTTP part endpoint is removed, including for internal-scoped JWTs and explicitly
unsecured APIs; internal peer authority comes only from mTLS. Wrong scope is 403, missing/invalid credential is 401 with
WWW-Authenticate. Neither refusal is retryable. Counters never label credentials.

A credential grants the stated scopes over its whole tenant, not row-level RBAC.
Branch/copy requires read authority over the source as well as destination write.
Own-tenant local sources require read scope; other tenant/store pairs require an
explicit signed source grant. Check branch, replication and external-copy sources
before source-store lookup or any engine call. The configured external-object prefix
policy remains in force. Authorized durable jobs continue until cancelled; token
expiry/key removal rejects future HTTP calls and does not revoke an accepted job.

User-directed transport revision: peers support both HTTP/2 over TLS/TCP and
HTTP/3 over QUIC/UDP, with one versioned MessagePack application protocol and
mutual TLS 1.3. Use the maintained Hyper HTTP/2 and h3/Quinn HTTP/3 stacks;
HTTP/3 library qualification is required, not inferred from protocol support.
No gRPC or raw TCP-only application transport. Peer calls never carry client bearer
tokens or minted JWTs. Both ends verify a deployment peer CA; clients verify the
server hostname/SAN. Peer certificates confer internal authority across tenants.
Use dedicated peer listeners on TCP and UDP with the same HTTPS authority and
`POST /peer/v1/part`, isolated from the public API. Both use `application/msgpack`.
MessagePack frames are bounded at 32 MiB and carry
protocol version, request ID, tenant and existing whole/open/scan payloads.
The length prefix is a big-endian u32; reject oversize before buffer allocation.
Exactly one frame is allowed per HTTP request/response body; reject trailing data.
Whole/open/scan semantics and local fallback remain intact; HTTP redirects,
HTTP/1, plaintext peers and unverified TLS are refused. ALPN is `h2` or `h3`.
`PSTORE_PEER_BIND`, `PSTORE_PEER_CA_FILE`, `PSTORE_PEER_CERT_FILE` and
`PSTORE_PEER_KEY_FILE` configure the listener and mTLS identity. Missing/invalid
material refuses configured peering before serving. Static/gossip URLs name the
peer listeners, not the public HTTP API. Reuse HTTP/2 and HTTP/3 connections and
their native stream multiplexing, including out-of-order replies. Bound total
resident connections across both transports at 64,
global active handlers at 256, per-connection in-flight RPCs at 64, and incoming
wire buffers at 64 MiB. Reclaim departed membership and idle connections; do not
serialize independent RPCs behind one connection-wide request/response lock.
Bound advertised receive credit on both transports at 64 KiB per stream and
256 KiB per connection; test HTTP/2 SETTINGS/WINDOW_UPDATE on the wire and
verify payloads larger than those windows complete without truncation/deadlock.
Expose explicit h2/h3/auto transport selection through `PSTORE_PEER_TRANSPORT`.
Auto prefers available HTTP/3 and
falls back to HTTP/2 when UDP/HTTP/3 is unavailable, within one overall deadline.
Cache bounded availability/backoff per current peer; never probe per tenant or
index. Certificate/authentication failures never trigger a fallback attempt.
Disable 0-RTT for all peer calls; held-part scan is stateful and cannot be replayed.
Bound decoded request headers at 8 KiB and disable HTTP/3 dynamic QPACK tables.
Refuse HTTP/3 encoded non-DATA frames above 32 KiB from the length prefix,
before the library can buffer their payload; apply this to both directions.
Transport
fallback preserves the same certificate trust and hostname verification.
Certificates and JWT keys rotate by operator restart with trust overlap, then
removal. Public HTTP still requires trusted TLS ingress; native peer mTLS is now
in scope. Authentication state remains immutable and bounded, not per tenant.

Dependency admission must pass the repository's existing advisory gate without
new ignores. Replace the archived rustls-pemfile wrapper with maintained
rustls-pki-types PEM APIs; the trust, input bounds and refusal contract stay the
same. Use fixed Quinn/Tokio patch releases for the audit findings recorded in
the verification ledger, and requalify the changed dependency tree.

## Acceptance criteria
1. A bare Api refuses unauthenticated HTTP with zero blob requests/bytes; standalone auth configuration fails closed unless keys or explicit private mode are provided.
2. Valid tokens resolve the declared u128 tenant with or without a matching header; forged/malformed tenant headers, wrong signatures/algorithms/issuer/audience/key IDs, expired/future/oversized/duplicate credentials are refused before engine access.
3. Public read/write/list/admin scope fixtures cover every registered public HTTP method; wrong-scope mutation, admin and metrics requests are 403 with no blob I/O or state changes. Peer authority and refusal of the removed public part route are verified by criterion 5.
4. Branch, replication and external-copy requests cannot read an ungranted tenant/store. Signed grants permit the intended source and do not grant another pair; refused requests create no destination or jobs.
5. Both HTTP/2 and HTTP/3 mTLS peers execute MessagePack vector and phased text queries with matching results and actual remote work; negotiated transport is asserted. Missing/untrusted/expired client certificates, wrong server SAN/CA and plaintext cannot invoke parts. The legacy public HTTP part endpoint cannot execute with any JWT scope or unsecured configuration. UDP-unavailable auto mode uses mTLS HTTP/2 within its deadline; authentication failures use local fallback without transport retries.
6. Config/Debug/errors never emit signing keys or bearer credentials. Clock/claim bounds, maximum tenant IDs, single-header parsing and overlapping/removal key rotation have deterministic controls.
7. Existing fixtures explicitly opt into private mode and all prior assertions, budgets and public request semantics remain intact; server/workspace gates pass.
8. Record provisional JWT CPU/allocation cost, HTTP/2-versus-HTTP/3 MessagePack/mTLS cold/warm microbenchmarks and authenticated request digests; add no blob requests, LISTs or application peer phases. Report handshake, UDP-unavailable fallback and packet-loss costs separately.

## Test plan
1. `a_bare_api_refuses_http_before_engine_access`: run on M126 and observe an accepted write; config tests observe missing auth accepted by the old entry path. Mutation: silently select private mode.
2. `bearer_identity_is_bound_before_dispatch`: signed valid/invalid corpus, absent/mismatched headers and extreme tenant IDs, with total request/byte spies. Mutations: skip signature, issuer/audience/time or header-binding validation.
3. `route_scopes_fence_every_public_operation`: table-driven positive and negative role/route controls. Mutation: route registered without its scope or always deny valid credentials.
4. `source_grants_fence_branch_replication_and_external_copy`: seed two tenants/stores, deny without grants, allow exact pairs, query/list unchanged after refusal. Mutation: check only destination or cross-product separate tenant/store grants.
5. Migrate peer socket fixtures to mTLS HTTP/2 and HTTP/3 with MessagePack; assert negotiated transport, vector/phased equality and parts served. Test certificate/SAN refusal on both, UDP-blocked auto fallback, authentication failure without transport retry, disabled 0-RTT, header/body/frame bounds, legacy public HTTP refusal, native stream concurrency and membership reclamation. Mutations: disable verification, fake h3 with h2, retain public part execution, retry plaintext, replay scan or retain departed connections.
6. Pure key/claim/header policy tests and real signed JWT controls, including exact expiry and secret redaction; no sleep-based expiry tests. Mutation: accept unknown key/algorithm or dump config credentials.
7. Run existing server and all-feature workspace tests, fast gates and request/depth budgets. Fixture migration changes authorization setup only, with assertions preserved.
8. Standalone/ignored microbenchmark with matching compiler/dependency versions, repeated samples and outcome digests; no new CI timing threshold. Deterministic policy tests are gate-design rung 3; typed scopes/tenant principals are rung 1.

Coverage qualification keeps the existing 95% line/region floors. Test-only
measurement/qualification helpers use the existing test-directory convention;
file-loading, input-boundary and poisoned-pool recovery controls exercise actual
production behavior. No coverage ignore or threshold is added for this milestone.
Remaining qualification covers malformed responses, IPv6 peer authorities,
stream cancellation/split propagation and closed/exhausted admission resources.
Remote QUIC TLS alert and protocol-close codes must never permit an H2 retry;
only explicitly classified availability-close codes may do so.
Parser compatibility controls retain wide-object duplicate and scalar behavior
after removal of the public JSON peer route. Each new control must reject its
named bad mutation before the final coverage attempt.

Resumed coverage controls reject malformed certificate PEM even beside valid
material, refuse scoped dispatch without a principal, and preserve JSON range
validation in heading values. Verify each against a fail-open mutation. This
adds no public behavior, coverage exclusion or weaker threshold.

## RA budget
Authentication and route/source authorization add 0 W / 0 Rseq / 0 Rpar / 0 LIST.
No credential request per tenant/index/query, no new durable metadata, no new
sequential blob round or application peer phase. Measure cold TLS handshakes and
warm channel CPU/bytes separately; do not describe handshake RTTs as free.

## Risks
The previous default was anonymous: tests, demos and deployments must migrate
explicitly. Route scope binding, source authority and dual-transport mTLS peers must land
together. HS256 keys are trusted deployment secrets shared with serving processes;
Public ingress TLS and key/certificate distribution remain operator responsibilities. No managed
API-key issuance, per-document RBAC, fleet quota or compliance guarantee is claimed.

## Tasks
- M127.1 Add bearer policy, closed/default constructors, route/source enforcement and mTLS HTTP/2 plus HTTP/3 MessagePack peers; migrate fixtures and record verification.
