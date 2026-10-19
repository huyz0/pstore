# M107 — a GCS batch delete is one request

**Serves:** [M7a](../M7a/VERIFIED.md)'s named gap: *"The `delete_batch` fix does not help GCS.
`object_store` 0.14.1 maps GCS's `delete_stream` to one request per location -- the JSON API it
uses has no bulk delete -- so GC's request rate still scales with objects there."* No backlog
row.

## What is true today

These facts come from running fake-gcs-server 1.52.2 and 1.55 in Docker, after M106.

- **The JSON API does have a bulk form.** `POST /batch/storage/v1` takes a `multipart/mixed`
  body of up to 100 sub-requests. Each sub-request is a `DELETE
  /storage/v1/b/{bucket}/o/{name}`, and the response gives each one its own status. Both
  emulator versions honour it: a delete of `a` and `b` in one request returns `200`, `200`,
  and both objects are gone. A delete of an absent object returns `404` for that part alone.
- **`ObjectStoreBackend::new` over GCS bills one request for N.** `delete_batch` calls
  `delete_stream`, which sends N `DELETE`s, while `Accounted` bills one. So the bill is wrong
  and GC's request rate scales with objects. M46 fixed the same bug for Azure with the
  `azure` constructor.
- **GCS is still not a server backend, and this milestone does not make it one.** Both
  emulator versions return `200` to an upload whose `ifGenerationMatch` names an existing or a
  stale generation. `scripts/conformance.sh --gcs-precondition` is red on both, so a CAS
  built on it would lose writes. `Backend` stays without `gcs`. What this fixes is the adapter
  that the probe and any future GCS backend sit on.

## Delta

1. `ObjectStoreBackend::gcs(store, client, endpoint, bucket, caps)`, behind a new `gcs`
   feature that `compat` includes. `client` is an `object_store` `HttpClient`, so a test can
   log what it sends:
   - `max_batch_delete: 100`, the API's cap, and `delete_is_free: false`;
   - `delete_batch` sends **one** `POST {endpoint}/batch/storage/v1`, bearer-authenticated
     from the store's own credential provider;
   - every other method is unchanged.
2. A part answering `2xx` or `404` is a delete. Absent is deleted, as it is on
   `MemoryStore`. Any other part status fails the batch, naming the key and the status. So
   does a non-`2xx` for the batch itself, and a response that does not give one status per
   key.
3. Object names are percent-encoded in each part's path. `/` is `%2F`, since every key
   pstore writes has one.
4. `scripts/fake-gcs.sh`: starts fake-gcs-server 1.55 in Docker, creates the bucket, runs the
   ignored test, and stops it. `portable: no`, outside `gates.sh`, as `azurite.sh` is.

**Not changed:** `Backend`, the server, GC. GC already chunks at `max_batch_delete` (M7a).

## Acceptance criteria

1. **The encoding is pinned without an emulator**, in `cargo test -p pstore-blob`. For 3 keys
   the body has 3 parts, each `DELETE` to the encoded path, with distinct `Content-ID`s and the
   closing boundary. A key's `/` is encoded, and so is a character outside the unreserved set.
2. **The response is read part by part**, also without an emulator:
   - `200` and `204` delete; `404` deletes;
   - a `403` in the second of three parts fails, naming the second key and `403`;
   - two statuses for three keys fail;
   - a part with no status line fails.
3. **On the emulator, one request deletes 100 objects.** Through `Accounted`: 97 objects
   written, then one `delete_batch` of 100 keys, 3 of them absent. The batch costs 1
   delete-class request, and a `get` of each key is `NotFound`. Every request either client
   sends is logged, and the call's log is exactly `POST /batch/storage/v1`, with 0 `DELETE`s.
   The emulator's own log agrees. Seen red first against `ObjectStoreBackend::new`, with all
   100 present: 100 `DELETE`s, billed as 1.
4. **Over the cap is refused**: 101 keys are an error before any request, as on every
   backend.
5. **M7a's gap line** is struck through, linking here.
6. Gates: `scripts/gates.sh`, and `scripts/check-portable.sh` with the new script `portable: no`.

## Test plan

| # | Seen red | What it catches |
|---|---|---|
| 1, 2 | against stubs that encode nothing and accept any response | a body the API rejects; a failed part read as a delete |
| 3 | `ObjectStoreBackend::new`: 100 `DELETE`s | a request rate that scales with objects, and a bill that hides it |

## RA budget

GC's deletes go from one request per object to one per 100 on GCS. That is the M7a gap,
closed. No read path changes.

## Found while building it

- **The emulator does not take `object_store`'s writes.** `object_store` writes GCS through the
  XML API, `PUT /{bucket}/{name}`, and fake-gcs-server 1.52.2 and 1.55 both answer it `400
  invalid uploadType`, whatever the name. Its reads work. The test seeds through the JSON API's
  media upload instead, so no write through the adapter is evidenced here.
- **`ObjectStoreBackend::new` fails a GCS batch that names an absent key.** Its first red run,
  with 3 keys absent, stopped at the first one with `NotFound`, so a repeated delete fails
  there, where `MemoryStore` reads absent as deleted. The batch path
  reads a part's `404` as deleted, so `gcs` does not have it.

## Risks

- ⚠️ **Emulator evidence** (D-99): GCS's real batch endpoint also caps a batch's payload, and
  may throttle a batch that a single request would not. Real GCS is M0b.
- **A part's `404` is read as deleted.** Since M23, a key GC deletes is never rewritten, so
  absent cannot mean anything else.

## Tasks

- **M107.1**: the constructor, the encoder and parser, the script, the ledger.
