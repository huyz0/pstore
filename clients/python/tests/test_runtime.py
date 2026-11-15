"""The generated Python client's runtime (M267): retries, pagination, the async client and
base64 vectors.

The retry cases run against a stub HTTP server scripted per test; the rest against the real
server scripts/clients.sh starts (PSTORE_URL), in a tenant of their own.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import os
import socket
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "clients" / "python"))

import pstore_client  # noqa: E402

OK = (200, {}, {"duties": []})


def refusal(status, retryable, headers=None):
    return (status, headers or {},
            {"error": {"code": "x", "message": "m", "retryable": retryable}})


class Stub:
    """A local server answering each request with the next of `script`'s answers."""

    def __init__(self, script):
        self.script = list(script)
        self.requests = 0
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802
                stub.requests += 1
                status, headers, body = stub.script.pop(0)
                raw = json.dumps(body).encode()
                self.send_response(status)
                for k, v in headers.items():
                    self.send_header(k, v)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class Retries(unittest.TestCase):
    def run_script(self, script, **kw):
        stub = Stub(script)
        waits = []
        client = pstore_client.Client(stub.url, sleep=waits.append, **kw)
        try:
            try:
                got = client.list_duties()
            except pstore_client.PstoreError as e:
                got = e
            return got, stub.requests, waits
        finally:
            stub.close()

    def test_retryable_refusals_are_retried_with_backoff(self):
        got, requests, waits = self.run_script([refusal(503, True), refusal(503, True), OK])
        self.assertEqual(got, OK[2])
        self.assertEqual(requests, 3)
        self.assertEqual(len(waits), 2)
        self.assertTrue(0.375 <= waits[0] <= 0.5, waits)
        self.assertTrue(0.75 <= waits[1] <= 1.0, waits)

    def test_retry_after_is_waited(self):
        got, requests, waits = self.run_script([refusal(429, True, {"retry-after": "2"}), OK])
        self.assertEqual((got, requests, waits), (OK[2], 2, [2.0]))
        got, _, waits = self.run_script([refusal(503, True, {"retry-after": "600"}), OK])
        self.assertEqual(waits, [pstore_client.MAX_RETRY_AFTER])

    def test_a_refusal_not_retryable_is_raised_at_once(self):
        got, requests, waits = self.run_script([refusal(400, False), OK])
        self.assertIsInstance(got, pstore_client.PstoreError)
        self.assertEqual((got.status, requests, waits), (400, 1, []))

    def test_max_retries_zero_sends_once(self):
        got, requests, waits = self.run_script([refusal(503, True), OK], max_retries=0)
        self.assertEqual((got.status, requests, waits), (503, 1, []))

    def test_the_last_error_is_raised_after_the_retries(self):
        got, requests, waits = self.run_script([refusal(503, True)] * 3 + [OK])
        self.assertEqual((got.status, requests, len(waits)), (503, 3, 2))

    def test_a_refused_connection_is_retried(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        waits = []
        client = pstore_client.Client(f"http://127.0.0.1:{port}", sleep=waits.append)
        with self.assertRaises(OSError):
            client.list_duties()
        self.assertEqual(len(waits), 2)


class Vectors(unittest.TestCase):
    def test_base64_round_trips_little_endian(self):
        floats = [0.0, -1.5, 3.25e-8, 1e30]
        import struct
        want = list(struct.unpack("<4f", struct.pack("<4f", *floats)))
        self.assertEqual(pstore_client.decode_vector(pstore_client.encode_vector(floats)), want)
        self.assertEqual(pstore_client.encode_vector([1.0, 2.0]), "AACAPwAAAEA=")
        self.assertEqual(pstore_client.decode_vector("AACAPwAAAEA="), [1.0, 2.0])
        with self.assertRaises(ValueError):
            pstore_client.decode_vector("AACA")


def server():
    url = os.environ.get("PSTORE_URL")
    if not url:
        raise unittest.SkipTest("PSTORE_URL names the server: run scripts/clients.sh")
    tenant = str(int(os.environ.get("PSTORE_FIXTURE_TENANT", "101")) + 1000)
    return url, tenant


class AgainstTheServer(unittest.TestCase):
    def test_iter_indexes_follows_every_page(self):
        url, tenant = server()
        client = pstore_client.Client(url, tenant=tenant)
        names = [f"p{i}" for i in range(5)]
        for n in names:
            client.create_index(n, {"dims": 2, "distance_metric": "cosine_distance"})
        pages = []
        original = client.list_indexes

        def counted(**kw):
            pages.append(kw.get("cursor"))
            return original(**kw)

        client.list_indexes = counted
        self.assertEqual(list(client.iter_indexes(prefix="p", page_size=2)), names)
        self.assertEqual(len(pages), 3)

    def test_async_client_answers_and_pages(self):
        url, tenant = server()
        client = pstore_client.AsyncClient(url, tenant=str(int(tenant) + 1))

        async def run():
            await client.write_documents("a", {"durability": "durable",
                                               "distance_metric": "euclidean_squared",
                                               "documents": [{"id": 1, "vector": [1.0, 0.0]}]})
            q = await client.query("a", {"vector": [1.0, 0.0], "top_k": 1})
            names = [n async for n in client.iter_indexes()]
            return q, names

        q, names = asyncio.run(run())
        self.assertEqual([r["id"] for r in q["results"]], [1])
        self.assertEqual(names, ["a"])

    def test_every_operation_has_its_async_twin(self):
        for op in pstore_client.Client.OPERATIONS:
            sync = inspect.signature(getattr(pstore_client.Client, op))
            twin = getattr(pstore_client.AsyncClient, op)
            self.assertTrue(inspect.iscoroutinefunction(twin), op)
            self.assertEqual(inspect.signature(twin).parameters, sync.parameters, op)

    def test_a_base64_vector_is_written_and_read_back(self):
        url, tenant = server()
        client = pstore_client.Client(url, tenant=str(int(tenant) + 2))
        floats = [0.25, -1.5, 3.0]
        client.write_documents("b", {"durability": "durable", "distance_metric": "euclidean_squared",
                                     "documents": [{"id": 1,
                                                    "vector": pstore_client.encode_vector(floats)}]})
        q = client.query("b", {"vector": floats, "top_k": 1, "include_vectors": ["vector"]})
        self.assertEqual(q["results"][0]["vector"], floats)
        q = client.query("b", {"vector": floats, "top_k": 1, "include_vectors": ["vector"],
                               "vector_encoding": "base64"})
        self.assertEqual(pstore_client.decode_vector(q["results"][0]["vector"]), floats)


if __name__ == "__main__":
    unittest.main()
