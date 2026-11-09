#!/usr/bin/env bash
# brain-wiki — cross-compile, assemble, verify, tag, (optionally) push.
#
# Why a script instead of `podman build -f Containerfile.prebuilt .`: the assemble-only
# recipe trusts that target/ and web/pkg/ are current, and nothing in Docker can check
# that. Every step below exists because one of those assumptions was wrong somewhere:
#
#   * compiling inside an amd64 container is not merely slow, it is broken — rustc
#     under qemu dies with `uncaught target signal 11`, even `rustc -vV`. So:
#     cargo-zigbuild natively, assemble in the recipe.   -> the cross-compile step
#   * a wasm-bindgen step that fails quietly leaves yesterday's bundle, and the image
#     tags successfully without the change                -> bundle freshness check
#   * podman's default OCI format silently drops HEALTHCHECK, so the container never
#     reports healthy                                     -> --format docker
#   * some registries reject the OCI manifest podman pushes by default
#                                                         -> --format=v2s2 on push
#   * a binary linked against a glibc newer than bookworm's fails at pod start, not
#     at build time                                       -> the .2.36 target pin
#   * an image missing the kit assets serves a healthy-looking API over a UI that 404s
#     its own bootstrap module                            -> app.mjs/kit.css in the checks
#   * stamping a git SHA onto a dirty tree makes the running service lie about itself
#                                                         -> the dirty-tree guard
#
# Usage:
#   tools/build-image.sh                # cross-build + assemble + verify; tags latest+<sha>
#   tools/build-image.sh --push         # also push both tags to the registry
#   tools/build-image.sh --skip-build   # assemble/verify what target/ + web/pkg already hold
#
# The checks below are deliberately stricter than `podman build -f
# Containerfile.prebuilt .`: that recipe trusts things no Dockerfile can verify.
# Keep them all, and keep a comment on each that says which failure it caught —
# a check nobody can justify is a check someone deletes.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT=$PWD
# The one place a registry is named. BRAIN_REGISTRY overrides it; nothing else in
# the repo hard-codes a host, because an image that can only be pushed to one
# registry is a build script that cannot be reused — and because the day a host is
# inlined in three files is the day two of them agree and the third ships a
# different image.
#
# The default is deliberately a namespace you must change: it is ghcr.io-shaped so
# `--push` fails loudly with a 401 on a repo that does not exist, rather than
# quietly succeeding against some registry the reader never meant to publish to.
REGISTRY=${BRAIN_REGISTRY:-ghcr.io/mikesmullin/brain-wiki}
GLIBC=${BRAIN_GLIBC:-2.36}
ARCH=${BRAIN_ARCH:-amd64}
case "$ARCH" in
  amd64) TRIPLE=x86_64-unknown-linux-gnu ;;
  arm64) TRIPLE=aarch64-unknown-linux-gnu ;;
  *) echo "BRAIN_ARCH must be amd64 or arm64, got $ARCH" >&2; exit 2 ;;
esac
# cargo-zigbuild takes the glibc pin as the target argument but writes under the plain
# triple, so conflating the two yields a build that succeeds and a script that then
# looks for a file that was never going to exist.
SERVER_BIN=target/${TRIPLE}/release/brain
WASM_TARGET=wasm32-unknown-unknown
WASM_BIN=target/${WASM_TARGET}/release/brain_app.wasm
PUSH=0; SKIP_BUILD=0
for a in "$@"; do
  case "$a" in
    --push) PUSH=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    *) echo "unknown argument: $a" >&2; exit 2 ;;
  esac
done

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die() { printf '\033[31mFAIL:\033[0m %s\n' "$*" >&2; exit 1; }

SHA=$(git rev-parse --short HEAD)

# The tag is the only answer to "which build is serving me?" that doesn't require
# logging into a cluster, so it is only worth applying if it identifies a real commit.
if [ -n "$(git status --porcelain --untracked-files=no)" ] && [ "${BRAIN_ALLOW_DIRTY:-0}" != 1 ]; then
  echo "tracked files are dirty — refusing to stamp :$SHA onto an image that does not" >&2
  echo "correspond to that commit. Commit first, or set BRAIN_ALLOW_DIRTY=1 and accept" >&2
  echo "that the tag will be lying about what is running." >&2
  git status --porcelain --untracked-files=no | sed 's/^/    /' >&2
  exit 1
fi

# The wasm-bindgen version is dictated by the kit, not by taste. A mismatched CLI emits
# bindings that fail in the browser at load rather than at build — the worst possible
# place to discover it, because the server log is silent and the page just stays blank.
WANT_WB=$(sed -n 's/^[[:space:]]*wasm-bindgen[[:space:]]*=[[:space:]]*"=\([0-9.]*\)".*/\1/p' vendor/rustex1/kit/Cargo.toml | head -1)
WANT_WB=${WANT_WB:-0.2.129}
WB_DIR=$(find tools -maxdepth 1 -type d -name "wasm-bindgen-${WANT_WB}-*" 2>/dev/null | head -1)
WB=${WB_DIR:-}/wasm-bindgen

say "preflight"
command -v podman >/dev/null || die "podman is not on PATH"
command -v cargo  >/dev/null || die "cargo is not on PATH"
podman info >/dev/null 2>&1 || die "cannot reach the podman socket (is the machine started?)"
[ -d vendor/rustex1/kit ] || die "vendor/rustex1 is empty — run: git submodule update --init --recursive"
if [ "$SKIP_BUILD" = 0 ]; then
  command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild is not on PATH (brew install cargo-zigbuild)"
  [ -x "$WB" ] || die "pinned wasm-bindgen ${WANT_WB} missing under tools/ (expected tools/wasm-bindgen-${WANT_WB}-*)"
fi
printf 'podman  : %s\nregistry: %s\narch    : %s (%s, glibc %s)\ncommit  : %s\nbindgen : %s\n' \
  "$(podman --version)" "$REGISTRY" "$ARCH" "$TRIPLE" "$GLIBC" "$SHA" "$WANT_WB"

if [ "$SKIP_BUILD" = 0 ]; then
  # Never compile under emulation. This is the line that replaces a Containerfile that
  # runs cargo: it worked on the native arch and produced `qemu: uncaught target
  # signal 11 (Segmentation fault)` on `rustc -vV` when asked for amd64.
  say "server (cross-compiled, not emulated)"
  cargo zigbuild --release --locked --target "${TRIPLE}.${GLIBC}" -p brain 2>&1 | tail -2

  say "app (wasm) + pinned bindings"
  cargo build --release --locked --target "$WASM_TARGET" -p brain-app 2>&1 | tail -2
  # rm -rf first: wasm-bindgen does not fail when a stale bundle is present, it leaves
  # files behind that the image then ships — which is why the freshness check below
  # compares against mtime and not merely against existence.
  rm -rf web/pkg
  "$WB" --target web --out-dir web/pkg "$WASM_BIN"
fi

say "bundle freshness"
# The check that would have caught the incident this script exists for. Comparing
# in-image hashes to on-disk ones only proves the image matches the tree — and in the
# stale-bundle case they DID match, because the build faithfully shipped the bundle
# wasm-bindgen had failed to regenerate. What distinguishes "current" is time: no
# input to the wasm may be newer than the bundle it produced.
[ -s web/pkg/brain_app_bg.wasm ] || die "web/pkg/brain_app_bg.wasm missing — run without --skip-build"
STALE=""
for src in $(find app/src web vendor/rustex1/kit/src -type f \( -name '*.rs' -o -name '*.css' \) 2>/dev/null); do
  [ "$src" -nt web/pkg/brain_app_bg.wasm ] && STALE="$STALE $src"
done
if [ -n "$STALE" ]; then
  echo "web/pkg/brain_app_bg.wasm is OLDER than:$STALE" >&2
  die "the wasm bundle is stale — wasm-bindgen did not run or failed; re-run without --skip-build"
fi

for f in "$SERVER_BIN" \
         web/pkg/brain_app_bg.wasm web/pkg/brain_app.js web/index.html web/styles.css \
         vendor/rustex1/kit/web/app.mjs vendor/rustex1/kit/web/kit.css \
         deploy/entrypoint.sh; do
  [ -s "$f" ] || die "required build input missing or empty: $f"
done
# Trust anchors are optional and never shipped: build/certs/ is tracked as a README
# alone, so a clean clone assembles fine and gets only the public roots. The
# directory itself must exist, though — both recipes COPY it, and a COPY of a
# missing directory fails the build with something unhelpful. On a network that
# intercepts TLS, drop the root CA PEM in here FIRST: the alternative is discovering
# it as rustls UnknownIssuer a hundred steps away from the cause.
[ -d build/certs ] || die "build/certs/ missing — the recipes COPY it into the trust store (see build/certs/README.md)"
ls build/certs/*.crt >/dev/null 2>&1 \
  || echo "    note: no trust anchor in build/certs/ (fine unless your network intercepts TLS)"

say "file types (proves the cross-compile targeted the right arch)"
file "$SERVER_BIN" | sed 's/^/    /'
file web/pkg/brain_app_bg.wasm | sed 's/^/    /'

say "stage image inputs"
# One named input directory for the assemble-only recipe. .containerignore excludes
# target/ for good reason (a host cargo tree is multi-GB and must never ride into a
# build context), so the binary is copied to a path that is deliberately in-scope.
# The recipe COPYs nothing else from target/, which means "not in build/out" and
# "not in the image" are the same statement.
rm -rf build/out && mkdir -p build/out
cp "$SERVER_BIN" build/out/brain
chmod 755 build/out/brain
[ -s build/out/brain ] || die "staging produced an empty binary"
ls -l build/out/brain | sed 's/^/    /'

say "build image ($REGISTRY:latest, :$SHA)"
# --format docker keeps HEALTHCHECK (podman's OCI default drops it).
podman build --format docker --platform=linux/"$ARCH" \
  -f Containerfile.prebuilt \
  -t "$REGISTRY:latest" -t "$REGISTRY:$SHA" . 2>&1 | tail -3

say "verify the image contains what is on disk"
# Necessary, not sufficient: it proves the image matches the tree, which the freshness
# check above is what makes meaningful. Catches the silent failure where COPY picks up
# a stale target/ from a previous arch or a previous commit.
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
for f in /opt/brain/brain /opt/brain/web/pkg/brain_app_bg.wasm /opt/brain/kit/app.mjs; do
  podman run --rm --entrypoint sha256sum "$REGISTRY:$SHA" "$f" 2>/dev/null | awk -v p="$f" '{print p" "$1}' >> "$tmp/in-image"
done
{
  # Compared against what was STAGED, which is what was COPYed — not against the
  # original target/ path the recipe never reads.
  printf '/opt/brain/brain %s\n'              "$(shasum -a 256 build/out/brain | cut -d' ' -f1)"
  printf '/opt/brain/web/pkg/brain_app_bg.wasm %s\n' "$(shasum -a 256 web/pkg/brain_app_bg.wasm | cut -d' ' -f1)"
  printf '/opt/brain/kit/app.mjs %s\n'        "$(shasum -a 256 vendor/rustex1/kit/web/app.mjs | cut -d' ' -f1)"
} > "$tmp/on-disk"
if ! diff -u "$tmp/on-disk" "$tmp/in-image" >/dev/null; then
  diff -u "$tmp/on-disk" "$tmp/in-image" >&2 || true
  die "image contents differ from the working tree"
fi
echo "    binary, wasm bundle and kit entry all match the tree"

if [ "$PUSH" = 1 ]; then
  # --format=v2s2 because some registries' Docker UI rejects the OCI manifest that
  # podman emits by default. Push both: :<sha> is the immutable one you deploy,
  # :latest is the convenience pointer that lets someone reproduce "what was
  # running" by date.
  say "push"
  for t in latest "$SHA"; do
    podman push --format=v2s2 "$REGISTRY:$t" 2>&1 | tail -1
  done
fi

say "done"
cat <<EOF
  $REGISTRY:$SHA   (also :latest)

  Deploy it by pointing your CD app (or a plain Deployment) at:
    image      $REGISTRY
    image_tag  $SHA          <- the immutable one; :latest is convenience only

  Whatever pins the image should pin the SHA, not :latest: the tag is the only
  answer to "which build is serving me?" that does not require logging into the
  cluster, and :latest in a Deployment makes that question unanswerable.
EOF
