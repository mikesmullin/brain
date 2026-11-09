#!/usr/bin/env bash
# brain dev server — Rust backend, Rust→wasm frontend, on the rustex1 kit.
#
#   ./dev.sh                 build if needed, serve on the port in rustex1.json
#   ./dev.sh 4000            override the port
#   ./dev.sh --rebuild       force a fresh wasm build on start
#   ./dev.sh --db <path>     serve a specific brain
#
# Saving a file under app/ rebuilds the wasm and hot-swaps it with no page
# reload; saving one under src/ rebuilds and re-execs this server. Both are the
# rustex1 watcher's doing, not this script's.
set -euo pipefail
cd "$(dirname "$0")"

if ! rustup target list --installed | grep -q wasm32-unknown-unknown; then
  echo "brain: adding the wasm32-unknown-unknown target"
  rustup target add wasm32-unknown-unknown
fi

# Pin the wasm-bindgen version the kit pins — a mismatched CLI produces a module
# the browser will not instantiate. Every fetch in this script is keyed to it.
V="$(grep -o 'wasm-bindgen = "=[0-9.]*"' vendor/rustex1/kit/Cargo.toml | grep -o '[0-9.]*' | head -1)"
V="${V:-0.2.129}"

# The dev server looks for a wasm-bindgen CLI in ./tools, vendor/rustex1/tools
# and ../rustex1/tools before falling back to PATH. Keep that promise.
have_cli() {
  ls tools/*/wasm-bindgen >/dev/null 2>&1 \
    || ls vendor/rustex1/tools/*/wasm-bindgen >/dev/null 2>&1 \
    || command -v wasm-bindgen >/dev/null 2>&1
}

if ! have_cli; then
  case "$(uname -s)" in
    Darwin) ARCH="$(uname -m)-apple-darwin" ;;
    *)      ARCH="$(uname -m)-unknown-linux-musl" ;;
  esac
  echo "brain: installing wasm-bindgen ${V} (${ARCH}) into tools/"
  mkdir -p tools
  curl -fsSL "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/${V}/wasm-bindgen-${V}-${ARCH}.tar.gz" \
    | tar xz -C tools/
fi

# build/vendor holds the pinned tarballs for the image build's hermetic path
# (Containerfile prefers them over a download, verifying against the .sha256sum
# committed beside each). Fetch both linux-musl archs — the container's uname -m
# decides which one it needs, and they are not re-fetchable from the image.
mkdir -p build/vendor
for A in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  T="build/vendor/wasm-bindgen-${V}-${A}.tar.gz"
  if [ ! -f "$T" ]; then
    echo "brain: fetching wasm-bindgen ${V} (${A}) into build/vendor/"
    curl -fsSL "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/${V}/wasm-bindgen-${V}-${A}.tar.gz" -o "$T"
  fi
  if [ -f "$T.sha256sum" ]; then
    # The committed digest is the provenance: a mismatch means the on-disk copy
    # is corrupt or swapped, and the image would fail at build time 100 steps away.
    if [ "$(shasum -a 256 "$T" | cut -d' ' -f1)" != "$(cut -d' ' -f1 "$T.sha256sum")" ]; then
      echo "brain: checksum mismatch for $T — re-fetch with: rm -f $T" >&2
      exit 1
    fi
  fi
done

# blaze if it is here (seconds off every incremental server rebuild), cargo if not
BUILD=cargo
command -v blaze >/dev/null 2>&1 && BUILD=blaze
$BUILD build -q -p brain

exec ./target/debug/brain web "$@"
