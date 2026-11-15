#!/usr/bin/env bash
# Public TLS and the gateway in front of a real pstore-server (M237).
#
# portable: no -- starts nginx and pstore-server on local ports and kills them; CI runs it on
# ubuntu with nginx installed.
#
# docs/deploy.md says to terminate public TLS in a trusted reverse proxy, and the plan's risk
# note says parser tests do not prove that a deployment does. So this runs the documented
# configuration, deploy/nginx/pstore-tls.conf, as written (only its at-sign placeholders filled
# in), in front of the real binary with signed-credential authentication (M127) on the
# in-memory store, and checks from a client's side:
#
#   1. a signed client creates, writes and strongly queries an index over HTTPS;
#   2. no credential is 401, an expired one 401, and a read-only one writing 403;
#   3. a valid credential with a forged tenant header is 403, and nothing lands in that tenant;
#   4. a tenant credential on an admin route is 403;
#   5. a body over pstore's own 2 MiB bound gets pstore's typed 413 through the proxy;
#   6. plain HTTP on the TLS port never reaches pstore;
#   7. pstore itself answers only on loopback: the proxy is its one public path.
#
# Every check prints one `ok` line; any failure exits non-zero.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/py.sh

for tool in nginx openssl; do
    command -v "$tool" >/dev/null || {
        echo "tls-smoke: $tool is required (CI installs nginx-light)" >&2
        exit 1
    }
done

cargo build --quiet -p pstore-server --bin pstore-server
BIN=target/debug/pstore-server

RUN=$(mktemp -d)
SERVER=""
cleanup() {
    [ -n "$SERVER" ] && kill "$SERVER" 2>/dev/null || true
    [ -f "$RUN/nginx.pid" ] && kill "$(cat "$RUN/nginx.pid")" 2>/dev/null || true
    wait 2>/dev/null || true
    # KEEP=1 keeps the logs and configuration for a failure's post-mortem.
    if [ -n "${KEEP:-}" ]; then echo "tls-smoke: kept $RUN" >&2; else rm -rf "$RUN"; fi
}
trap cleanup EXIT
mkdir -p "$RUN/body" "$RUN/proxy" "$RUN/fastcgi" "$RUN/uwsgi" "$RUN/scgi"
# Run as root (a container), nginx's workers drop to an unprivileged user and need its temp dirs.
chmod 755 "$RUN"
chmod 777 "$RUN/body" "$RUN/proxy" "$RUN/fastcgi" "$RUN/uwsgi" "$RUN/scgi"

free_port() {
    py -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}
API_PORT=$(free_port)
TLS_PORT=$(free_port)

# A throwaway CA and a server certificate for localhost, as an operator's PKI would issue.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=pstore smoke CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -keyout "$RUN/ca.key" -out "$RUN/ca.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
    -keyout "$RUN/server.key" -out "$RUN/server.csr" >/dev/null 2>&1
printf '%s\n' 'subjectAltName=DNS:localhost,IP:127.0.0.1' 'keyUsage=critical,digitalSignature' \
    'extendedKeyUsage=serverAuth' 'authorityKeyIdentifier=keyid' 'subjectKeyIdentifier=hash' \
    >"$RUN/san.ext"
openssl x509 -req -in "$RUN/server.csr" -CA "$RUN/ca.pem" -CAkey "$RUN/ca.key" \
    -CAcreateserial -days 1 -extfile "$RUN/san.ext" -out "$RUN/server.pem" >/dev/null 2>&1

SECRET=$(py -c 'import base64, os; print(base64.b64encode(os.urandom(32)).decode())')
AUTH="{\"issuer\":\"https://issuer.smoke\",\"audience\":\"pstore-smoke\",\"active_kid\":\"k1\",\"keys\":[{\"kid\":\"k1\",\"secret\":\"$SECRET\"}]}"

# M238: refused bodies drained, as docs/deploy.md asks of a server behind a proxy.
LIMITS=${SMOKE_LIMITS:-'{"version":1,"drain_refused_bodies":true}'}
PSTORE_AUTH_CONFIG="$AUTH" PSTORE_REQUEST_LIMITS_CONFIG="$LIMITS" PSTORE_LANE=1 PSTORE_BACKEND=memory \
    PSTORE_BIND="127.0.0.1:$API_PORT" "$BIN" >"$RUN/server.log" 2>&1 &
SERVER=$!

sed -e "s|@RUN@|$RUN|g" -e "s|@UPSTREAM@|127.0.0.1:$API_PORT|g" \
    -e "s|@LISTEN@|127.0.0.1:$TLS_PORT|g" -e "s|@CERT@|$RUN/server.pem|g" \
    -e "s|@KEY@|$RUN/server.key|g" deploy/nginx/pstore-tls.conf >"$RUN/nginx.conf"
if grep -q '@[A-Z]*@' "$RUN/nginx.conf"; then
    echo "tls-smoke: a placeholder was left unfilled" >&2
    exit 1
fi
nginx -t -q -e "$RUN/error.log" -p "$RUN" -c "$RUN/nginx.conf" 2>"$RUN/nginx-t.log" || {
    cat "$RUN/nginx-t.log" >&2
    exit 1
}
nginx -e "$RUN/error.log" -p "$RUN" -c "$RUN/nginx.conf"

SMOKE_SECRET="$SECRET" SMOKE_CA="$RUN/ca.pem" SMOKE_TLS_PORT="$TLS_PORT" \
    SMOKE_API_PORT="$API_PORT" SMOKE_SERVER_LOG="$RUN/server.log" py - <<'PY'
import base64, hashlib, hmac, http.client, json, os, socket, ssl, sys, time

SECRET = base64.b64decode(os.environ["SMOKE_SECRET"])
TLS_PORT = int(os.environ["SMOKE_TLS_PORT"])
API_PORT = int(os.environ["SMOKE_API_PORT"])
CTX = ssl.create_default_context(cafile=os.environ["SMOKE_CA"])


def b64(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def token(tenant, scopes, exp_in=600):
    header = b64(json.dumps({"alg": "HS256", "typ": "JWT", "kid": "k1"}).encode())
    claims = {"iss": "https://issuer.smoke", "aud": "pstore-smoke",
              "exp": int(time.time()) + exp_in, "tenant": str(tenant), "scopes": scopes}
    body = b64(json.dumps(claims).encode())
    sig = hmac.new(SECRET, f"{header}.{body}".encode(), hashlib.sha256).digest()
    return f"{header}.{body}.{b64(sig)}"


def call(method, path, body=None, auth=None, headers=None):
    conn = http.client.HTTPSConnection("localhost", TLS_PORT, context=CTX, timeout=10)
    h = {"content-type": "application/json"}
    if auth:
        h["authorization"] = f"Bearer {auth}"
    h.update(headers or {})
    data = body if isinstance(body, (bytes, type(None))) else json.dumps(body).encode()
    conn.request(method, path, body=data, headers=h)
    res = conn.getresponse()
    raw = res.read()
    conn.close()
    try:
        return res.status, json.loads(raw) if raw else None
    except ValueError:
        return res.status, raw.decode(errors="replace")


def check(name, ok, detail=""):
    if not ok:
        print(f"FAIL {name}: {detail}", file=sys.stderr)
        sys.exit(1)
    print(f"ok {name}")


rw = token(9, ["read", "write", "list"])
last = None
for _ in range(100):
    try:
        last = call("GET", "/v1/indexes", auth=rw)
        if last[0] == 200:
            break
    except OSError as e:
        last = e
    time.sleep(0.1)
else:
    print(open(os.environ["SMOKE_SERVER_LOG"]).read(), file=sys.stderr)
    sys.exit(f"tls-smoke: pstore did not answer through the proxy: {last}")

# 1. A signed client, end to end over HTTPS.
s, b = call("PUT", "/v1/indexes/smoke", {"dims": 2}, auth=rw)
check("an index is created over HTTPS", s == 200, f"{s} {b}")
docs = {"durability": "durable", "documents": [{"id": "a", "vector": [1.0, 0.0]}]}
s, b = call("PUT", "/v1/indexes/smoke/documents", docs, auth=rw)
check("a durable write over HTTPS", s == 200, f"{s} {b}")
q = {"vector": [1.0, 0.0], "top_k": 1, "consistency": "strong"}
s, b = call("POST", "/v1/indexes/smoke/query", q, auth=rw)
check("a strong query over HTTPS returns the row", s == 200 and b["results"][0]["id"] == "a",
      f"{s} {b}")
s, b = call("GET", "/v1/indexes", auth=rw, headers={"x-pstore-tenant": "9"})
check("a tenant header matching the credential is accepted", s == 200, f"{s} {b}")

# 2. Credentials.
s, b = call("GET", "/v1/indexes")
check("no credential is 401", s == 401, f"{s} {b}")
s, b = call("GET", "/v1/indexes", auth=token(9, ["read", "list"], exp_in=-120))
check("an expired credential is 401", s == 401, f"{s} {b}")
s, b = call("PUT", "/v1/indexes/smoke/documents", docs, auth=token(9, ["read"]))
check("a read-only credential writing is 403", s == 403, f"{s} {b}")

# 3. A forged tenant header: refused, and nothing lands in that tenant.
s, b = call("PUT", "/v1/indexes/stolen/documents", docs, auth=rw,
            headers={"x-pstore-tenant": "10"})
check("a forged tenant header is 403", s == 403, f"{s} {b}")
s, b = call("GET", "/v1/indexes", auth=token(10, ["list"]))
check("the forged write landed nowhere", s == 200 and "stolen" not in json.dumps(b), f"{s} {b}")

# 4. Admin routes need the admin scope.
s, b = call("POST", "/v1/admin/fold", {}, auth=rw)
check("a tenant credential on an admin route is 403", s == 403, f"{s} {b}")

# 5. Bodies: refused early, at the bound, over it. pstore refuses a bad credential before it
# reads the body; buffered by the proxy, that refusal still reaches the client.
# Whitespace pads a body without nearing pstore's string or array bounds.
# M238: drained, the refusal survives at every size up to the bound.
for kib in (256, 1536, 2048):
    early = b'{"documents": []' + b" " * ((kib << 10) - 17) + b"}"
    s, b = call("PUT", "/v1/indexes/smoke/documents", early)
    check(f"a {kib} KiB body with no credential is pstore's 401", s == 401
          and isinstance(b, dict) and "error" in b, f"{s} {str(b)[:200]}")
at = b'{"documents": []' + b" " * ((2 << 20) - 17) + b"}"
assert len(at) == 2 << 20
s, b = call("PUT", "/v1/indexes/smoke/documents", at, auth=rw)
check("a body of exactly 2 MiB reaches pstore and is answered", s in (200, 400)
      and "request_too_large" not in json.dumps(b), f"{s} {str(b)[:200]}")
over = at + b" "
s, proxied = call("PUT", "/v1/indexes/smoke/documents", over, auth=rw)
direct = http.client.HTTPConnection("127.0.0.1", API_PORT, timeout=10)
direct.request("PUT", "/v1/indexes/smoke/documents", body=over,
               headers={"authorization": f"Bearer {rw}", "content-type": "application/json"})
res = direct.getresponse()
own = json.loads(res.read())
check("one byte over gets pstore's own typed 413 from the proxy",
      s == 413 and res.status == 413 and proxied == own, f"{s} {proxied} against {own}")
s, b = call("PUT", "/v1/indexes/smoke/documents", over + b" " * (1 << 20), auth=rw)
check("a 3 MiB body gets the same typed 413", s == 413 and b == own, f"{s} {str(b)[:200]}")

# 6. Plain HTTP on the TLS port is answered by the proxy and never forwarded.
conn = http.client.HTTPConnection("127.0.0.1", TLS_PORT, timeout=10)
conn.request("GET", "/v1/indexes", headers={"authorization": f"Bearer {rw}"})
res = conn.getresponse()
raw = res.read().decode(errors="replace")
check("plain HTTP on the TLS port is refused by the proxy",
      res.status == 400 and "pstore" not in raw.lower() and "indexes" not in raw, f"{res.status}")

# 7. pstore answers only on loopback.
probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    probe.connect(("192.0.2.1", 9))
    addr = probe.getsockname()[0]
except OSError:
    addr = "127.0.0.1"  # no route anywhere: nothing but loopback to try
finally:
    probe.close()
if addr.startswith("127."):
    print("NOT-RUN pstore answers only on loopback: this host has no other address")
else:
    try:
        socket.create_connection((addr, API_PORT), timeout=2).close()
        reached = True
    except OSError:
        reached = False
    check(f"pstore does not answer on {addr}", not reached)
PY
echo "tls-smoke: every check passed"
