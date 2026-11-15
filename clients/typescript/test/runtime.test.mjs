// The generated TypeScript client's runtime (M267): retries, pagination and base64 vectors.
//
// The retry cases run against a stub `fetch` scripted per test, and a refused connection
// against a closed local port; the rest against the real server scripts/clients.sh starts
// (PSTORE_URL), in a tenant of their own.
import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:net";
import { Client, PstoreError, MAX_RETRY_AFTER, encodeVector, decodeVector } from "../src/index.ts";

const OK = [200, {}, { duties: [] }];
const refusal = (status, retryable, headers = {}) =>
  [status, headers, { error: { code: "x", message: "m", retryable } }];

async function run(script, options = {}) {
  let requests = 0;
  const waits = [];
  const fetch = async () => {
    requests++;
    const [status, headers, body] = script.shift();
    return new Response(JSON.stringify(body), { status, headers });
  };
  const client = new Client({ baseUrl: "http://stub/", fetch, sleep: async (ms) => { waits.push(ms); }, ...options });
  let got;
  try {
    got = await client.listDuties();
  } catch (e) {
    got = e;
  }
  return { got, requests, waits };
}

test("retryable refusals are retried with backoff", async () => {
  const { got, requests, waits } = await run([refusal(503, true), refusal(503, true), OK]);
  assert.deepEqual(got, OK[2]);
  assert.equal(requests, 3);
  assert.equal(waits.length, 2);
  assert.ok(waits[0] >= 375 && waits[0] <= 500, String(waits));
  assert.ok(waits[1] >= 750 && waits[1] <= 1000, String(waits));
});

test("Retry-After is waited, at most MAX_RETRY_AFTER", async () => {
  let r = await run([refusal(429, true, { "retry-after": "2" }), OK]);
  assert.deepEqual([r.got, r.requests, r.waits], [OK[2], 2, [2000]]);
  r = await run([refusal(503, true, { "retry-after": "600" }), OK]);
  assert.deepEqual(r.waits, [MAX_RETRY_AFTER * 1000]);
});

test("a refusal not retryable is raised at once", async () => {
  const { got, requests, waits } = await run([refusal(400, false), OK]);
  assert.ok(got instanceof PstoreError);
  assert.deepEqual([got.status, requests, waits], [400, 1, []]);
});

test("maxRetries 0 sends once", async () => {
  const { got, requests, waits } = await run([refusal(503, true), OK], { maxRetries: 0 });
  assert.deepEqual([got.status, requests, waits], [503, 1, []]);
});

test("the last error is raised after the retries", async () => {
  const { got, requests, waits } = await run([refusal(503, true), refusal(503, true), refusal(503, true), OK]);
  assert.deepEqual([got.status, requests, waits.length], [503, 3, 2]);
});

test("a refused connection is retried", async () => {
  const port = await new Promise((done) => {
    const s = createServer();
    s.listen(0, "127.0.0.1", () => {
      const p = s.address().port;
      s.close(() => done(p));
    });
  });
  const waits = [];
  const client = new Client({ baseUrl: `http://127.0.0.1:${port}`, sleep: async (ms) => { waits.push(ms); } });
  await assert.rejects(client.listDuties(), TypeError);
  assert.equal(waits.length, 2);
});

test("base64 vectors round trip little-endian", () => {
  const floats = [0, -1.5, 3.25e-8, 1e30];
  assert.deepEqual(decodeVector(encodeVector(floats)), Float32Array.from(floats));
  assert.equal(encodeVector([1, 2]), "AACAPwAAAEA=");
  assert.deepEqual(Array.from(decodeVector("AACAPwAAAEA=")), [1, 2]);
  assert.throws(() => decodeVector("AACA"));
});

function server() {
  const url = process.env.PSTORE_URL;
  assert.ok(url, "PSTORE_URL names the server: run scripts/clients.sh");
  return { url, tenant: String(Number(process.env.PSTORE_FIXTURE_TENANT ?? "102") + 1000) };
}

test("listIndexesAll follows every page", async () => {
  const { url, tenant } = server();
  const client = new Client({ baseUrl: url, tenant });
  const names = ["p0", "p1", "p2", "p3", "p4"];
  for (const n of names) await client.createIndex(n, { dims: 2, distance_metric: "cosine_distance" });
  let pages = 0;
  const original = client.listIndexes.bind(client);
  client.listIndexes = (o) => { pages++; return original(o); };
  const got = [];
  for await (const n of client.listIndexesAll({ prefix: "p", page_size: 2 })) got.push(n);
  assert.deepEqual(got, names);
  assert.equal(pages, 3);
});

test("a base64 vector is written and read back", async () => {
  const { url, tenant } = server();
  const client = new Client({ baseUrl: url, tenant: String(Number(tenant) + 2) });
  const floats = [0.25, -1.5, 3];
  await client.writeDocuments("b", { durability: "durable", distance_metric: "euclidean_squared",
    documents: [{ id: 1, vector: encodeVector(floats) }] });
  let q = await client.query("b", { vector: floats, top_k: 1, include_vectors: ["vector"] });
  assert.deepEqual(q.results[0].vector, floats);
  q = await client.query("b", { vector: floats, top_k: 1, include_vectors: ["vector"], vector_encoding: "base64" });
  assert.deepEqual(Array.from(decodeVector(q.results[0].vector)), floats);
});
