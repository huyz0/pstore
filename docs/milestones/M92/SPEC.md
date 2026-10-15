# M92 — Copy an external index to a new name

**Serves:** backlog row 79. **Status: planned.** No owner decision is needed beyond [M86](../M86/SPEC.md)'s:
a copy reads the customer's object only where the *copying* deployment has allowed it.

## What this is, and what it deliberately is not

[M86](../M86/SPEC.md) left replication of an external index out: a replica follows a source by
copying its segments as they change, and a registration is not that. It is **immutable once made**
(a changed registration is a drop and a new one), so the honest equivalent is a **one-shot copy**,
as `branch` is for an index, not a continuous follower. A copy that must follow a source's later
re-registration is a second copy, which is cheap: it moves the codes blob and rebuilds nothing.

`Engine::copy_external(dest, source, from)`: `source` is a [`ReplicaSource`] (store, tenant,
index), as a replication names one, and `from` is the store it names.

1. The deployment must allow external sources (M86's default is off, and refusal makes no
   request).
2. Read the source tenant's HEAD from `from`; the named external index must be in it.
3. The registration's `source` must be under **this** deployment's allowed prefixes: the copy
   is refused, by name, if not. A deployment that has not allowed the customer's bucket does not
   read it because another deployment did.
4. `dest` must be a free name (M86's rule).
5. The codes blob is read from `from`, checked to be a codes blob, written under the copy's own
   derived key, and HEAD is committed naming it. **The customer's object is neither read nor
   written**, and nothing is rebuilt.

## What a copy does and does not promise

The codes blob pins the customer's object by key, length and CAS tag. A copy queried in the
same bucket reads the same object and answers as its source does, to the bit. A copy made from
**another store** reads the object of the same key *in its own store*: if there is none, a query is
`Source` ("no object"), and if there is one that is not byte-for-byte the pinned object (an S3
single-part ETag is the content's MD5, so a copied object matches; a multipart one does not) a
query is `Changed`. Never a stale answer; this milestone does not try to make a different bucket
hold the right object.

## Delta

- `Engine::copy_external`; `register_external`'s commit loop becomes the one both use.
- `PUT /v1/indexes/{dest}/external/copy` with `{"index": "x", "tenant": "71", "store": "eu"}`
  (`tenant` defaults to the caller's, `store` to this server's own bucket, as a replication's
  source does). `404 external_index_not_found` when the source has none; `400` for a store the
  server does not know; M86's `403`/`409` otherwise.

## Acceptance criteria

1. `a_copy_answers_as_its_source_does`: in this tenant under a new name, and from another tenant,
   a query by the copy's name equals the source's, to the bit; **nothing of the customer's object
   is read** while copying (the counting store sees no ranged read of it), and the copy's HEAD
   names a codes blob of its own.
2. `a_copy_from_another_store_reads_the_customers_object_in_its_own`: from a second store,
   with the object in the copying store it answers as the source does; without it, a query is
   refused by name, never answered.
3. `a_copy_is_refused_unless_this_deployment_allows_it`: off, or the registration's source
   outside the copying deployment's prefixes, is refused before the codes are read.
4. `a_copy_is_refused_with_its_reason`: no such source index, a name taken, a source that is
   not a codes blob, a source store that fails.
5. `a_copy_outlives_its_source_and_buries_only_its_own_codes`: dropping the source leaves the
   copy answering; dropping the copy buries its codes and not the source's.
6. Over the API: `a_copy_over_the_api`, with each refusal's status.
7. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Size:** M. Touches `pstore-engine` (`external.rs`) and `pstore-server` (`external.rs`).
