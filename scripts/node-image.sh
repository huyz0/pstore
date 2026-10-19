#!/usr/bin/env bash
# M110 — the node image's runtime stage needs no network.
#
#   scripts/node-image.sh            build the runtime stage offline and check its CA bundle
#   scripts/node-image.sh --old REV  the same, with the runtime stage as it was at REV
#
# ⚠️ It builds `dev/Dockerfile.node`'s runtime stage (everything from the last `FROM` on),
# verbatim, behind a stage that only holds a binary, and it builds with `--network=none`. A
# `RUN apt-get` there fails loudly instead of depending on whether a Debian mirror answers
# today. The binary comes from `cargo build --offline` inside the build stage's own image, so
# it links against the same glibc the runtime has. No step needs the network once the two base
# images are local.
# ⚠️ It does not build the build stage. That stage fetches crates, and where a proxy re-signs
# TLS it needs that proxy's CA, which is the host's configuration rather than the image's.
# portable: no -- Docker, and a bind mount of the host's cargo registry.
set -euo pipefail
cd "$(dirname "$0")/.."
export MSYS_NO_PATHCONV=1

BUILD_IMAGE=$(awk '$1 == "FROM" && $3 == "AS" && $4 == "build" { print $2 }' dev/Dockerfile.node)
stage() { awk 'FNR == NR { if ($1 == "FROM") last = FNR; next } FNR >= last' "$1" "$1"; }
REGISTRY=${CARGO_HOME:-$HOME/.cargo}/registry

case "${1:-}" in
  "") ;;
  --old) [ -n "${2:-}" ] || { echo "usage: $0 [--old REV]" >&2; exit 2; } ;;
  *) echo "usage: $0 [--old REV]" >&2; exit 2 ;;
esac

tag=pstore-node-image-check
work=$(mktemp -d)
trap 'rm -rf "$work" || true; docker rmi -f "$tag" >/dev/null 2>&1 || true' EXIT
# ⚠️ `--old REV` swaps the RUNTIME stage only; the build image still comes from the current
# file, which is what the check needs: the stage under test is the one that changed.
if [ "${1:-}" = "--old" ]; then git show "$2:dev/Dockerfile.node" >"$work/Dockerfile.repo"
else cp dev/Dockerfile.node "$work/Dockerfile.repo"; fi

toolchain=$(docker run --rm "$BUILD_IMAGE" rustc -V | awk '{ print $2 }')
[ -n "$toolchain" ] || { echo "FAIL: no rustc in $BUILD_IMAGE" >&2; exit 1; }
# ⚠️ As the caller's uid, not the image's root. Under rootful Docker a root container leaves
# root-owned files in `$work`, and the trap's `rm` then fails for a non-root caller -- which,
# under `set -e`, turned a passing check into exit 1 and leaked a release build per run.
mkdir -p "$work/target" "$work/ctx" "$work/cargo/registry"
docker run --rm --network none --user "$(id -u):$(id -g)" -e RUSTUP_TOOLCHAIN="$toolchain" \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/t -e CARGO_PROFILE_RELEASE_DEBUG=0 \
  -v "$PWD:/src:ro" -v "$work/target:/t" -v "$work/cargo:/cargo" -v "$REGISTRY:/cargo/registry:ro" -w /src \
  "$BUILD_IMAGE" cargo build --release --offline -q -p pstore-node
cp "$work/target/release/pstore-node" "$work/ctx/"
{ printf 'FROM %s AS build\nCOPY pstore-node /src/target/release/pstore-node\n' "$BUILD_IMAGE"
  stage "$work/Dockerfile.repo"; } >"$work/ctx/Dockerfile"

docker build --network=none -q -t "$tag" "$work/ctx" >/dev/null
want=$(docker run --rm --entrypoint sha256sum "$BUILD_IMAGE" /etc/ssl/certs/ca-certificates.crt | awk '{ print $1 }')
got=$(docker run --rm --entrypoint sha256sum "$tag" /etc/ssl/certs/ca-certificates.crt | awk '{ print $1 }')
[ "$want" = "$got" ] || { echo "FAIL: CA bundle $got, the build image has $want" >&2; exit 1; }
echo "ok: runtime stage built with no network; CA bundle $got matches $BUILD_IMAGE"
