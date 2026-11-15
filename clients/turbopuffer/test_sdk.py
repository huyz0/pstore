"""The vendor's own Python SDK, turbopuffer 3.1.0, against pstore's facade (M268).

Run by scripts/facade.sh, which starts a server and installs the pinned wheel; PSTORE_URL
names the server. Each step's answer is also read from the native API, which the facade
must translate, not reinterpret.
"""

from __future__ import annotations

import json
import os
import unittest
import urllib.request

import turbopuffer

TENANT = "201"


def native(path: str, body: dict | None = None) -> dict:
    """A native call in the same tenant."""
    req = urllib.request.Request(
        os.environ["PSTORE_URL"] + path,
        data=None if body is None else json.dumps(body).encode(),
        method="GET" if body is None else ("PUT" if path.endswith("/documents") else "POST"),
        headers={"content-type": "application/json", "x-pstore-tenant": TENANT},
    )
    with urllib.request.urlopen(req, timeout=30) as res:
        return json.loads(res.read())


def client() -> turbopuffer.Turbopuffer:
    url = os.environ.get("PSTORE_URL")
    if not url:
        raise unittest.SkipTest("PSTORE_URL names the server: run scripts/facade.sh")
    return turbopuffer.Turbopuffer(api_key=TENANT, base_url=url, telemetry=False)


ROWS = [
    {"id": 1, "vector": [1.0, 0.0], "title": "red fox jumps", "n": 3, "kind": "a"},
    {"id": 2, "vector": [0.8, 0.6], "title": "lazy dog sleeps", "n": 1, "kind": "a"},
    {"id": 3, "vector": [0.0, 1.0], "title": "red red dog", "n": 2, "kind": "b"},
]


class Sdk(unittest.TestCase):
    def test_the_sdk_against_the_facade(self):
        tpuf = client()
        ns = tpuf.namespace("sdk-docs")
        w = ns.write(upsert_rows=ROWS, distance_metric="euclidean_squared",
                     schema={"title": {"type": "string", "full_text_search": True}})
        self.assertEqual((w.status, w.rows_affected), ("OK", 3))
        native("/v1/admin/fold", {})

        ann = ns.query(rank_by=("vector", "ANN", [1.0, 0.1]), top_k=3, include_attributes=True)
        want = native("/v1/indexes/sdk-docs/query", {"vector": [1.0, 0.1], "top_k": 3,
                                                     "include_attributes": True,
                                                     "consistency": "strong"})
        self.assertEqual([r.id for r in ann.rows], [r["id"] for r in want["results"]])
        self.assertEqual([r["$dist"] for r in ann.rows], [r["$dist"] for r in want["results"]])
        self.assertEqual(ann.rows[0]["title"], "red fox jumps")

        bm25 = ns.query(rank_by=("title", "BM25", "red"), top_k=10)
        self.assertEqual({r.id for r in bm25.rows}, {1, 3})
        self.assertEqual(bm25.rows[0].id, 3)

        order = ns.query(rank_by=("n", "desc"), top_k=10, filters=("kind", "Eq", "a"),
                         include_attributes=["n"])
        self.assertEqual([(r.id, r["n"]) for r in order.rows], [(1, 3), (2, 1)])

        per = ns.query(rank_by=("vector", "ANN", [1.0, 0.0]),
                       limit={"total": 3, "per": {"attributes": ["kind"], "limit": 1}},
                       include_attributes=["kind"])
        self.assertEqual(sorted(r["kind"] for r in per.rows), ["a", "b"])

        eventual = ns.query(rank_by=("vector", "ANN", [1.0, 0.0]), top_k=1,
                            consistency={"level": "eventual", "max_staleness_seconds": 5})
        self.assertEqual([r.id for r in eventual.rows], [1])

        meta = ns.metadata()
        self.assertEqual(meta.approx_row_count, 3)
        self.assertTrue(ns.exists())

        tpuf.namespace("sdk-more").write(upsert_rows=[{"id": 1, "vector": [1.0, 0.0]}],
                                         distance_metric="cosine_distance")
        names = [n.id for n in tpuf.namespaces(prefix="sdk-", page_size=1)]
        self.assertEqual(names, ["sdk-docs", "sdk-more"])

        w = ns.write(patch_rows=[{"id": 1, "n": 9}], deletes=[2])
        self.assertEqual((w.rows_patched, w.rows_deleted), (1, 1))
        native("/v1/admin/fold", {})
        after = ns.query(rank_by=("id", "asc"), top_k=10, include_attributes=["n"])
        self.assertEqual([(r.id, r["n"]) for r in after.rows], [(1, 9), (3, 2)])

        self.assertEqual(ns.delete_all().status, "OK")
        self.assertFalse(ns.exists())

    def test_a_refusal_is_the_sdks_error(self):
        tpuf = client()
        with self.assertRaises(turbopuffer.NotFoundError):
            tpuf.namespace("sdk-nothing").metadata()
        with self.assertRaises(turbopuffer.BadRequestError) as e:
            tpuf.namespace("sdk-bad").write(delete_by_filter=("id", "Eq", 1))
        self.assertIn("delete_by_filter", str(e.exception))


if __name__ == "__main__":
    unittest.main()
