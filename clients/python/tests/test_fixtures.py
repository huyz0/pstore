"""The native API's semantic fixtures, run through the generated Python client (M221).

Run by scripts/clients.sh against a server it starts; PSTORE_URL names it. Each step of
api/fixtures/native.json calls the client method its operationId names, with its path
parameters, query and body, and must answer the step's status and hold its `expect` -- the
same steps, and the same rule, as crates/pstore-server/tests/it/openapi.rs.
"""

from __future__ import annotations

import json
import os
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "clients" / "python"))

import pstore_client  # noqa: E402


def holds(got, want, at, out):
    """`got` holds `want`: an object at least its keys, an array exactly its items in order."""
    if isinstance(want, dict) and isinstance(got, dict):
        for k, v in want.items():
            if k in got:
                holds(got[k], v, f"{at}.{k}", out)
            else:
                out.append(f"{at}.{k} is missing")
    elif isinstance(want, list) and isinstance(got, list):
        if len(got) != len(want):
            out.append(f"{at}: {len(got)} items, not {len(want)}")
        else:
            for i, (g, w) in enumerate(zip(got, want)):
                holds(g, w, f"{at}[{i}]", out)
    elif isinstance(want, bool) or isinstance(got, bool):
        if got is not want:
            out.append(f"{at}: {got!r}, not {want!r}")
    elif isinstance(want, (int, float)) and isinstance(got, (int, float)):
        if float(got) != float(want):
            out.append(f"{at}: {got!r}, not {want!r}")
    elif got != want:
        out.append(f"{at}: {got!r}, not {want!r}")


def pointer(doc, ptr):
    """Whether the JSON pointer `ptr` names a value in `doc`."""
    for part in ptr.split("/")[1:]:
        if isinstance(doc, list) and part.isdigit() and int(part) < len(doc):
            doc = doc[int(part)]
        elif isinstance(doc, dict) and part in doc:
            doc = doc[part]
        else:
            return False
    return True


class Fixtures(unittest.TestCase):
    def test_every_step_through_the_client(self):
        url = os.environ.get("PSTORE_URL")
        self.assertTrue(url, "PSTORE_URL names the server: run scripts/clients.sh")
        fixtures = json.loads((ROOT / "api" / "fixtures" / "native.json").read_text())
        client = pstore_client.Client(url, tenant=fixtures["tenant"])
        failures = []
        called = set()
        for step in fixtures["steps"]:
            name, op = step["name"], step["operation"]
            method = getattr(client, op, None)
            if method is None:
                failures.append(f"{name}: the client has no {op}")
                continue
            called.add(op)
            args = dict(step.get("path", {}))
            if "body" in step:
                args["body"] = step["body"]
            args.update(step.get("query", {}))
            try:
                got, status = method(**args), client.last_status
            except pstore_client.PstoreError as e:
                got, status = e.body, e.status
            if status != step["status"]:
                failures.append(f"{name}: {status}, not {step['status']}: {got}")
                continue
            if status >= 400 and not isinstance(got, dict):
                failures.append(f"{name}: a refusal without its error body")
            if "expect" in step:
                holds(got, step["expect"], name, failures)
            for ptr in step.get("absent", []):
                if pointer(got, ptr):
                    failures.append(f"{name}: {ptr} must be absent")
        self.assertEqual(failures, [])
        self.assertEqual(called, set(pstore_client.Client.OPERATIONS))

    def test_a_refusal_carries_its_code_and_retryability(self):
        url = os.environ.get("PSTORE_URL")
        self.assertTrue(url)
        client = pstore_client.Client(url, tenant="8")
        with self.assertRaises(pstore_client.PstoreError) as e:
            client.get_index("nothing_here")
        self.assertEqual((e.exception.status, e.exception.code, e.exception.retryable),
                         (404, "index_not_found", False))

    def test_a_write_returns_the_session_a_read_then_passes(self):
        url = os.environ.get("PSTORE_URL")
        self.assertTrue(url)
        client = pstore_client.Client(url, tenant="9")
        w = client.write_documents("s", {"durability": "durable", "distance_metric": "euclidean_squared",
                                         "documents": [{"id": 1, "vector": [1.0, 0.0]}]})
        self.assertEqual(client.last_session, w["session"])
        q = client.query("s", {"vector": [1.0, 0.0], "consistency": "session"}, session=w["session"])
        self.assertEqual([r["id"] for r in q["results"]], [1])


class Plumbing(unittest.TestCase):
    def test_query_values_are_written_as_the_server_reads_them(self):
        self.assertEqual([pstore_client._qv(v) for v in (True, False, 1000, "x")],
                         ["true", "false", "1000", "x"])

    def test_retry_after_is_read_with_or_without_an_envelope(self):
        import io
        import urllib.error
        from email.message import Message

        def refuse(body):
            headers = Message()
            headers["retry-after"] = "7"
            def opener(req, timeout):
                raise urllib.error.HTTPError(req.full_url, 503, "x", headers, io.BytesIO(body))
            return opener

        client = pstore_client.Client("http://unused")
        for body, code in [(b'{"error": {"code": "not_folded", "message": "m", "retryable": true}}',
                            "not_folded"), (b"<html>gateway</html>", "unparsed")]:
            original = pstore_client.urllib.request.urlopen
            pstore_client.urllib.request.urlopen = refuse(body)
            try:
                with self.assertRaises(pstore_client.PstoreError) as e:
                    client.list_duties()
            finally:
                pstore_client.urllib.request.urlopen = original
            self.assertEqual((e.exception.code, e.exception.retry_after, e.exception.retryable),
                             (code, 7, True))


if __name__ == "__main__":
    unittest.main()
