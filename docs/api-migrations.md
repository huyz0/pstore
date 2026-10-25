# Native API migration notes

## M127: explicit authentication and encrypted peers

Standalone startup now requires `PSTORE_AUTH_CONFIG`, or the explicit
`PSTORE_ALLOW_UNAUTHENTICATED=1` private/test mode. Configure existing deployments
before updating the server. The two modes cannot be combined. Rust embeddings
choose authenticated constructors or the explicitly unsecured constructors;
bare `Api::new` and `with_cache` expose closed HTTP surfaces.

Public requests use a signed bearer token containing the tenant and explicit
operation scopes. An optional tenant header must agree with that identity.
Branch/copy/replication source access also requires read authority for the exact
source tenant/store pair. See the [authentication contract](milestones/M127/AUTH.md)
for claims, grants and key rotation. Accepted durable jobs continue until cancelled.

The public `/v1/internal/part` route is removed, including in private mode.
Configure a dedicated peer TCP/UDP address, deployment peer CA, certificate and
key. Static/gossip URLs now use HTTPS and name the peer listener. Both HTTP/2
and HTTP/3 carry versioned MessagePack over mTLS 1.3; `PSTORE_PEER_TRANSPORT`
selects outgoing `h2`, `h3` or `auto`. Public bearer tokens do not authorize peers.
Legacy peer builds cannot speak this protocol and cause local fallback during
an upgrade; plan configuration and capacity for that transition.

Public HTTP still needs trusted TLS ingress. This change does not add fleet
quotas or change native mutation/durability/read defaults. Current evidence and
remaining qualification are in [M127 verification](milestones/M127/VERIFIED.md).

## M126: unknown request fields are errors

Write/query request roots, document envelopes, and multi-query children reject unknown
keys with HTTP 400 and `error.code: "bad_request"`. The error message names the field.
Previously these envelopes could ignore an extra key, so an unsupported option or typo
could appear to succeed. Existing strict nested options keep their rejection behavior.

Remove unsupported options or correct the named key before retrying. For example,
`durabilty` is an error; the supported field is `durability`, with `"batched"` or
`"durable"`. Vendor-only options such as `return_affected_ids`, `disable_backpressure`,
`compute_attributes`, and `limit` are not accepted by this native API version.
Use `top_k` to set the native query result limit.
Use the [comparison](comparisons/turbopuffer-2026-10-25.md) for the pinned capability gaps
and [delivery status](comparisons/DELIVERY.md) for work in progress.

User data maps remain open. Attribute names under `attributes`, named vector fields,
patch columns, and aggregate labels do not become reserved merely because they resemble
request option names. Known duplicate keys retain the existing last-value behavior.

Invalid requests are validated before engine access and cannot partially buffer or
flush their valid rows. No stored-data migration is needed. Native write/read defaults
remain batched/eventual; this change does not add authentication or change mutation
completion guarantees. The [M126 verification ledger](milestones/M126/VERIFIED.md)
records the evidence and any checks still pending.
