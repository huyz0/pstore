// The native API's semantic fixtures, run through the generated TypeScript client (M222).
//
// Run by scripts/clients.sh against a server it starts; PSTORE_URL names it. Each step of
// api/fixtures/native.json calls the client method its operationId names, and must answer the
// step's status and hold its `expect` -- the same steps, and the same rule, as the Rust
// conformance test and the Python client's suite.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { Client, OPERATIONS, PstoreError } from "../src/index.ts";

const fixtures = JSON.parse(
  readFileSync(new URL("../../../api/fixtures/native.json", import.meta.url), "utf8"),
);

function url() {
  const u = process.env.PSTORE_URL;
  assert.ok(u, "PSTORE_URL names the server: run scripts/clients.sh");
  return u;
}

// `got` holds `want`: an object at least its keys, an array exactly its items in order.
function holds(got, want, at, out) {
  if (Array.isArray(want)) {
    if (!Array.isArray(got) || got.length !== want.length) {
      out.push(`${at}: ${JSON.stringify(got)}, not ${want.length} items`);
      return;
    }
    want.forEach((w, i) => holds(got[i], w, `${at}[${i}]`, out));
  } else if (want !== null && typeof want === "object") {
    if (got === null || typeof got !== "object" || Array.isArray(got)) {
      out.push(`${at}: ${JSON.stringify(got)}, not an object`);
      return;
    }
    for (const [k, w] of Object.entries(want)) {
      if (k in got) holds(got[k], w, `${at}.${k}`, out);
      else out.push(`${at}.${k} is missing`);
    }
  } else if (got !== want) {
    out.push(`${at}: ${JSON.stringify(got)}, not ${JSON.stringify(want)}`);
  }
}

// Whether the JSON pointer `ptr` names a value in `doc`.
function pointer(doc, ptr) {
  for (const part of ptr.split("/").slice(1)) {
    if (doc === null || typeof doc !== "object" || !(part in doc)) return false;
    doc = doc[part];
  }
  return true;
}

test("every step through the client", async () => {
  const client = new Client({ baseUrl: url(), tenant: process.env.PSTORE_FIXTURE_TENANT ?? fixtures.tenant });
  const failures = [];
  const called = new Set();
  for (const step of fixtures.steps) {
    const op = OPERATIONS[step.operation];
    if (!op || typeof client[op.method] !== "function") {
      failures.push(`${step.name}: the client has no ${step.operation}`);
      continue;
    }
    called.add(step.operation);
    const args = op.path.map((p) => step.path[p]);
    if (op.body) args.push(step.body);
    if (op.options) args.push(step.query ?? {});
    else if (step.query) failures.push(`${step.name}: ${step.operation} takes no query`);
    const fn = client[op.method];
    let got;
    let status;
    try {
      got = await fn.apply(client, args);
      status = client.lastStatus;
    } catch (e) {
      if (!(e instanceof PstoreError)) throw e;
      got = e.body;
      status = e.status;
    }
    if (status !== step.status) {
      failures.push(`${step.name}: ${status}, not ${step.status}: ${JSON.stringify(got)}`);
      continue;
    }
    if (step.expect) holds(got, step.expect, step.name, failures);
    for (const ptr of step.absent ?? []) {
      if (pointer(got, ptr)) failures.push(`${step.name}: ${ptr} must be absent`);
    }
  }
  assert.deepEqual(failures, []);
  assert.deepEqual([...called].sort(), Object.keys(OPERATIONS).sort());
});

test("a refusal carries its code and retryability", async () => {
  const client = new Client({ baseUrl: url(), tenant: "18" });
  await assert.rejects(client.getIndex("nothing_here"), (e) =>
    e instanceof PstoreError && e.status === 404 && e.code === "index_not_found" && e.retryable === false);
});

test("a write returns the session a read then passes", async () => {
  const client = new Client({ baseUrl: url(), tenant: "19" });
  const w = await client.writeDocuments("s", {
    durability: "durable", distance_metric: "euclidean_squared",
    documents: [{ id: 1, vector: [1, 0] }],
  });
  assert.equal(client.lastSession, w.session);
  const q = await client.query("s", { vector: [1, 0], consistency: "session" }, { session: w.session });
  assert.deepEqual(q.results.map((r) => r.id), [1]);
});

test("Retry-After is read with or without an envelope, and query values are written as text", async () => {
  for (const [body, code] of [
    ['{"error": {"code": "not_folded", "message": "m", "retryable": true}}', "not_folded"],
    ["<html>gateway</html>", "unparsed"],
  ]) {
    let asked;
    const fetch = async (url) => {
      asked = url;
      return new Response(body, { status: 503, headers: { "retry-after": "7" } });
    };
    const client = new Client({ baseUrl: "http://unused/", fetch });
    await assert.rejects(client.getIndex("i", { progress: true }), (e) =>
      e instanceof PstoreError && e.code === code && e.retryAfter === 7 && e.retryable === true);
    assert.equal(asked, "http://unused/v1/indexes/i?progress=true");
  }
});

test("a call sends its method, body, bearer token, tenant and session", async () => {
  let asked;
  const fetch = async (url, init) => {
    asked = { url, init };
    return new Response('{"results": []}', { status: 200, headers: { "x-pstore-session": "s2" } });
  };
  const client = new Client({ baseUrl: "http://unused/", token: "t", tenant: "5", fetch });
  await client.query("a b", { vector: [1, 0] }, { session: "s1" });
  assert.equal(asked.url, "http://unused/v1/indexes/a%20b/query");
  assert.equal(asked.init.method, "POST");
  assert.deepEqual(JSON.parse(asked.init.body), { vector: [1, 0] });
  const h = asked.init.headers;
  assert.equal(h["authorization"], "Bearer t");
  assert.equal(h["x-pstore-tenant"], "5");
  assert.equal(h["x-pstore-session"], "s1");
  assert.equal(h["content-type"], "application/json");
  assert.equal(client.lastSession, "s2");
});
