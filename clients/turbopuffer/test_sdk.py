"""The vendor's own Python SDK, turbopuffer 3.1.0, against pstore's facade (M268).

Run by scripts/facade.sh, which starts a server and installs the pinned wheel; PSTORE_URL
names the server. Each step's answer is also read from the native API, which the facade
must translate, not reinterpret.
"""

from __future__ import annotations

import json
import os
import time
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
        # The native schema records a type only for datetime, uuid, bytes and uint64
        # attributes (M173), so schema() is exactly its attributes, empty here.
        summary = native("/v1/indexes/sdk-docs")
        self.assertEqual(ns.schema(), summary["schema"]["attributes"])

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

    def test_writes_by_filter_and_by_condition(self):
        """M269: each applied, each answered with the rows it changed."""
        tpuf = client()
        ns = tpuf.namespace("sdk-cond")
        ns.write(upsert_rows=ROWS, distance_metric="euclidean_squared")
        w = ns.write(delete_by_filter=("kind", "Eq", "b"))
        self.assertEqual((w.rows_deleted, w.rows_affected), (1, 1))
        w = ns.write(patch_by_filter={"filters": ("kind", "Eq", "a"), "patch": {"n": 7}})
        self.assertEqual(w.rows_patched, 2)
        w = ns.write(upsert_rows=[{"id": 1, "vector": [1.0, 0.0], "n": 8},
                                  {"id": 2, "vector": [0.8, 0.6], "n": 8}],
                     upsert_condition=("id", "Eq", 1), return_affected_ids=True,
                     distance_metric="euclidean_squared")
        self.assertEqual((w.rows_upserted, w.upserted_ids), (1, [1]))
        w = ns.write(patch_rows=[{"id": 1, "n": 9}, {"id": 2, "n": 9}],
                     patch_condition=("n", "Eq", 7), return_affected_ids=True)
        self.assertEqual((w.rows_patched, w.patched_ids), (1, [2]))
        w = ns.write(deletes=[1, 2], delete_condition=("n", "Eq", 8),
                     return_affected_ids=True)
        self.assertEqual((w.rows_deleted, w.deleted_ids, w.patched_ids), (1, [1], None))
        w = ns.write(upsert_rows=[{"id": 5, "vector": [0.5, 0.5]}], return_affected_ids=True,
                     distance_metric="euclidean_squared")
        self.assertEqual(w.upserted_ids, [5])
        native("/v1/admin/fold", {})
        got = ns.query(rank_by=("id", "asc"), top_k=10, include_attributes=["n"])
        self.assertEqual([(r.id, getattr(r, "n", None)) for r in got.rows], [(2, 9), (5, None)])
        ns.delete_all()

    def test_multi_query(self):
        """M269: one result a sub-query, each the native query's rows."""
        tpuf = client()
        ns = tpuf.namespace("sdk-multi")
        ns.write(upsert_rows=ROWS, distance_metric="euclidean_squared")
        native("/v1/admin/fold", {})
        got = ns.multi_query(
            queries=[{"rank_by": ("vector", "ANN", [1.0, 0.1]), "top_k": 2},
                     {"rank_by": ("n", "asc"), "top_k": 3, "include_attributes": ["n"]}],
            consistency={"level": "eventual", "max_staleness_seconds": 5})
        ann = native("/v1/indexes/sdk-multi/query", {"vector": [1.0, 0.1], "top_k": 2,
                                                     "consistency": "strong"})
        self.assertEqual(len(got.results), 2)
        self.assertEqual([r.id for r in got.results[0].rows], [r["id"] for r in ann["results"]])
        self.assertEqual([(r.id, r["n"]) for r in got.results[1].rows], [(2, 1), (3, 2), (1, 3)])
        with self.assertRaises(turbopuffer.BadRequestError) as e:
            ns.multi_query(queries=[{"rank_by": ("n", "asc"), "top_k": 1}],
                           rerank_by=["RRF"])
        self.assertIn("rerank_by", str(e.exception))
        ns.delete_all()

    def test_explain_warm_recall_schema_and_metadata(self):
        """M270: each one native call, or two for update_metadata."""
        tpuf = client()
        ns = tpuf.namespace("sdk-ops")
        ns.write(upsert_rows=ROWS, distance_metric="euclidean_squared")
        native("/v1/admin/fold", {})
        plan = ns.explain_query(rank_by=("vector", "ANN", [1.0, 0.1]), top_k=2)
        self.assertIn('"dense"', plan.plan_text)
        self.assertEqual(ns.hint_cache_warm().status, "ACCEPTED")
        r = ns.recall(num=2, top_k=2, include_ground_truth=True)
        self.assertTrue(0.0 <= r.avg_recall <= 1.0)
        self.assertEqual(len(r.ground_truth), 2)
        for g in r.ground_truth:
            self.assertTrue(all(isinstance(n["$dist"], float) for n in g.nearest_neighbors))
        with self.assertRaises(turbopuffer.BadRequestError) as e:
            ns.recall(num=1, rank_by=("vector", "ANN", [1.0, 0.0]))
        self.assertIn("rank_by", str(e.exception))
        with self.assertRaises(turbopuffer.BadRequestError) as e:
            ns.update_schema(schema={"n": {"type": "int", "filterable": True}})
        self.assertIn("schema_immutable", str(e.exception))
        meta = ns.update_metadata(read_only=True)
        self.assertEqual(meta.approx_row_count, 3)
        self.assertGreater(meta.approx_logical_bytes, 0)
        with self.assertRaises(turbopuffer.BadRequestError) as e:
            ns.write(upsert_rows=[{"id": 9, "vector": [1.0, 0.0]}],
                     distance_metric="euclidean_squared")
        self.assertIn("index_read_only", str(e.exception))
        ns.update_metadata(read_only=False)
        ns.write(upsert_rows=[{"id": 9, "vector": [1.0, 0.0]}],
                 distance_metric="euclidean_squared")
        with self.assertRaises(turbopuffer.BadRequestError) as e:
            ns.update_metadata(pinning=True)
        self.assertIn("pinning", str(e.exception))
        ns.delete_all()

    def test_copy_branch_and_the_async_copy(self):
        """M271: each the native branch or copy. Its namespaces are cp-, not sdk-: another
        test lists the sdk- prefix."""
        tpuf = client()
        src = tpuf.namespace("cp-src")
        src.write(upsert_rows=ROWS, distance_metric="euclidean_squared")
        native("/v1/admin/fold", {})

        def ids_and_n(ns):
            got = ns.query(rank_by=("id", "asc"), top_k=10, include_attributes=["n"])
            return [(r.id, getattr(r, "n", None)) for r in got.rows]

        source = ids_and_n(src)
        self.assertEqual(len(source), 3)
        branch = tpuf.namespace("cp-branch")
        self.assertEqual(branch.branch_from(source_namespace="cp-src").rows_affected, 0)
        self.assertEqual(ids_and_n(branch), source)
        copy = tpuf.namespace("cp-copy")
        copy.copy_from(source_namespace="cp-src")
        self.assertEqual(ids_and_n(copy), source)
        copy.write(patch_rows=[{"id": 1, "n": 99}])
        native("/v1/admin/fold", {})
        self.assertEqual(ids_and_n(src), source)
        self.assertNotEqual(ids_and_n(copy), source)
        by_write = tpuf.namespace("cp-by-write")
        by_write.write(copy_from_namespace="cp-src")
        self.assertEqual(ids_and_n(by_write), source)

        later = tpuf.namespace("cp-later")
        token = later.start_copy_from(source_namespace="cp-src").token
        deadline = time.monotonic() + 60
        while True:
            op = later.poll_copy_from(token=token)
            if op.status == "finished" or time.monotonic() > deadline:
                break
            time.sleep(0.5)
        self.assertEqual(op.status, "finished")
        # No billing is invented, so the SDK cannot type the result and hands back its dict.
        result = op.result if isinstance(op.result, dict) else op.result.model_dump()
        self.assertEqual(result["success"]["status"], "OK")
        self.assertEqual(ids_and_n(later), source)
        with self.assertRaises(turbopuffer.NotFoundError):
            later.poll_copy_from(token="999999")
        with self.assertRaises(turbopuffer.BadRequestError) as e:
            tpuf.namespace("cp-far").copy_from(source_namespace="cp-src",
                                                source_region="aws-us-east-1")
        self.assertIn("source_region", str(e.exception))
        for ns in (src, branch, copy, by_write, later):
            ns.delete_all()

    def test_the_manifest_names_everything_the_sdk_sends(self):
        """M272: every namespace method, write key and query key of the pinned SDK."""
        import inspect
        from pathlib import Path

        from turbopuffer.resources.namespaces import NamespacesResource
        from turbopuffer.types import namespace_query_params, namespace_write_params

        manifest = json.loads(
            (Path(__file__).resolve().parents[2] / "api/compat/turbopuffer.json").read_text())
        self.assertEqual(manifest["sdk"]["version"], turbopuffer.__version__)
        methods = {m for m, v in inspect.getmembers(NamespacesResource)
                   if not m.startswith("_") and inspect.isfunction(v)}
        self.assertEqual(methods | {"namespaces"}, set(manifest["operations"]))
        for params, keys in [
            (namespace_write_params.NamespaceWriteParams, "write_keys"),
            (namespace_query_params.NamespaceQueryParams, "query_keys"),
        ]:
            sent = set(params.__annotations__) - {"namespace"}
            self.assertEqual(sent, set(manifest[keys]), keys)

    def test_a_refusal_is_the_sdks_error(self):
        tpuf = client()
        with self.assertRaises(turbopuffer.NotFoundError):
            tpuf.namespace("sdk-nothing").metadata()
        with self.assertRaises(turbopuffer.BadRequestError) as e:
            tpuf.namespace("sdk-bad").write(upsert_rows=[], sharding={"num_shards": 2})
        self.assertIn("sharding", str(e.exception))


if __name__ == "__main__":
    unittest.main()
