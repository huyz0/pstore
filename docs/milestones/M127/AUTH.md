# M127 authentication contract

Implementation and verification are tracked in [SPEC.md](SPEC.md) and
[VERIFIED.md](VERIFIED.md). This describes the intended operator contract;
NOT-RUN criteria remain incomplete.

The standalone server requires one explicit authentication mode. Configure
`PSTORE_AUTH_CONFIG` with a JSON object:

```json
{
  "issuer": "https://identity.example.org/pstore",
  "audience": "pstore-production",
  "active_kid": "october-2026",
  "keys": [
    {"kid": "october-2026", "secret": "BASE64_OF_AT_LEAST_32_RANDOM_BYTES"}
  ]
}
```

The placeholder is not a signing key. Supply an independently generated random
secret through the deployment's secret facility. The server and trusted issuer
share HS256 keys. Anyone who holds a signing key can issue credentials for any
tenant, including administrative and internal credentials. Ordinary clients
receive signed tokens, never the key configuration.

Tokens carry a `kid` header and these claims:

```json
{
  "iss": "https://identity.example.org/pstore",
  "aud": "pstore-production",
  "exp": 1793059200,
  "tenant": "9",
  "scopes": ["read", "write"],
  "sources": [{"tenant": "10", "store": "far"}]
}
```

`exp` is an integer Unix timestamp in seconds; the example expires and must be
replaced when issuing a token. Optional `nbf` uses the same units. The tenant is
a decimal string so JSON number precision cannot change a 128-bit identity.
Send the token in one `Authorization: Bearer ...` header. A tenant header can be
omitted; if supplied, it must match the signed tenant.

Scopes are independent. `read` permits queries, metadata, warming and quarantine
reads. `write` permits mutations, including schema, replication and external
index changes. `list` permits index and replication enumeration. `admin` permits
administrative operations and process-wide metrics. `internal` is accepted as a
claim for compatibility but grants no public route or peer authority. An administrator receives only the scopes explicitly issued. Each scope
covers the token's whole tenant.

Branch and copy operations also require source authority. Reading the caller's
own local source requires `read`. Other sources require an exact signed
tenant/store pair in `sources`; the empty store name means the local store.
Independent entries do not authorize the cross product of their tenants and
stores. Deployment external-object prefix restrictions still apply.

Rotate keys by restarting servers with both old and new IDs, then issue new
credentials and set the new `active_kid` to match the issuer. After clients have
migrated, restart with the old ID removed. Removing a key or expiring
a token rejects future calls. An already authorized durable replication job
continues until cancelled through its API.

`PSTORE_ALLOW_UNAUTHENTICATED=1` selects private/test mode and cannot be combined
with `PSTORE_AUTH_CONFIG`. Embedded private fixtures use explicitly unsecured
constructors. A bare `Api` is closed until constructed with an explicit policy.

Terminate public HTTP TLS at a trusted ingress. Peer transport uses a separate
HTTP/2 (TLS/TCP) and HTTP/3 (QUIC/UDP) listeners carrying MessagePack, with mutual
TLS, a deployment peer CA, and server SAN
verification. Peer certificates grant internal authority across tenants; use a
dedicated CA and keep it separate from ordinary API client identities. Peers
never receive public bearer tokens or signing keys. The old HTTP part endpoint
is removed even in private mode. This milestone does not provide managed key
distribution, audit retention or per-document permissions.

Both peer listeners use the same HTTPS authority and `POST /peer/v1/part` with
`application/msgpack`. Configure a dedicated peer address with `PSTORE_PEER_BIND`
and the identity with `PSTORE_PEER_CA_FILE`, `PSTORE_PEER_CERT_FILE` and
`PSTORE_PEER_KEY_FILE`. Static and gossip peer URLs name this address. TLS 1.3
is required and 0-RTT is disabled. `PSTORE_PEER_TRANSPORT` supports `h2`, `h3` and
`auto`; auto prefers HTTP/3 and uses HTTP/2 when HTTP/3 is unavailable. A failed
certificate check never triggers a transport retry. The coordinator executes a
failed peer's work locally under the existing search fallback semantics.

The MessagePack schema preserves query and score bits. BSON's additional value
types are unnecessary for these typed whole/open/scan messages. No BSON codec
or gRPC layer is part of this protocol. Distributed-query integration and
provisional transport benchmark evidence are recorded in the verification ledger.
