# brain-wiki — the brain wiki as a deployable web service: three stages, one process, one port.
#
# Local (Apple Silicon, matches a KIND cluster):
#   podman build --format docker --platform=linux/arm64 -f Containerfile \
#     -t localhost:5001/brain-wiki:$(git rev-parse --short HEAD) .
#   podman push --tls-verify=false localhost:5001/brain-wiki:<tag>
# Publish (production clusters are amd64):
#   podman build --format docker --platform=linux/amd64 -f Containerfile \
#     -t "$REGISTRY:$(git rev-parse --short HEAD)" .
#   podman push --format=v2s2 "$REGISTRY:$(git rev-parse --short HEAD)"
#
# $REGISTRY resolves in tools/build-image.sh (BRAIN_REGISTRY) — the only place a registry is named.
# --format=v2s2 on push and --format docker on build: podman's OCI default drops HEALTHCHECK.
# Nothing pins --platform here, so one file builds both arches; the wasm-bindgen download below resolves its own.

# ── build ───────────────────────────────────────────────────────────────────
# Pin to -bookworm, not a floating tag: the build libc is what the binary links against, and a base that drifts past the runtime's bookworm yields `GLIBC_2.39' not found.
FROM rust:1-slim-bookworm AS build

# Optional TLS anchors for networks that intercept crates.io/rust-lang.org: build/certs/ ships empty, and public roots stay for github.com, which is not intercepted.
COPY build/certs/ /usr/local/share/ca-certificates/
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tar build-essential pkg-config \
 && update-ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# rustup and cargo speak rustls and read the OS store only through SSL_CERT_FILE; without this they keep their bundled webpki roots and every intercepted download fails with UnknownIssuer.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
COPY . .

# Two artifacts from one tree — the server binary and the wasm bundle.
# Everything Rust-facing runs in THIS run, after the COPY, twice for load-bearing reasons.
# 1. the wasm-bindgen pin is read from vendor/rustex1/kit/Cargo.toml, which does not exist until the COPY — a toolchain step before it can only hardcode a fallback that silently decouples the CLI from the crate.
# 2. `rustup target add` must run with rust-toolchain.toml on disk, or the target lands on the pinned default toolchain while cargo picks the floating stable, and the build dies with E0463 "can't find crate for `core`".
# The sed reads `wasm-bindgen = "=0.2.129"` — space between = and " — and `test -n` plus the version comparison are what turn an empty match into a build error rather than a fallback pin.
# --locked is not decoration: Cargo.lock is committed, so a divergent dependency graph fails here instead of shipping a binary nobody ran.
# No pipe and no `set -o pipefail` in this RUN: /bin/sh is dash, which rejects the option outright, so the tarball lands in a file first — which also makes a failed fetch fail as curl rather than as a truncated stream in tar.
# Cap cargo's parallelism: on a laptop this is stability, not micro-optimization — competing with a running KIND cluster for an 8 GiB VM dropped the libkrun socket mid-link. 0 = cargo decides (right on CI).
ARG CARGO_BUILD_JOBS=0
RUN set -eu \
 && if [ "$CARGO_BUILD_JOBS" != "0" ]; then export CARGO_BUILD_JOBS; fi \
 && rustup show active-toolchain \
 && rustup target add wasm32-unknown-unknown \
 && rustup target list --installed | grep -qx wasm32-unknown-unknown \
 && V="$(sed -n 's/^[[:space:]]*wasm-bindgen[[:space:]]*=[[:space:]]*"=\([0-9.]*\)".*/\1/p' vendor/rustex1/kit/Cargo.toml | head -1)" \
 && test -n "$V" || { echo "could not read the wasm-bindgen pin from the kit" >&2; exit 1; } \
 && case "$(uname -m)" in \
      x86_64)  WB="x86_64-unknown-linux-musl" ;; \
      aarch64) WB="aarch64-unknown-linux-musl" ;; \
      *) echo "unsupported build arch: $(uname -m)" >&2; exit 1 ;; \
    esac \
 && TARBALL="build/vendor/wasm-bindgen-${V}-${WB}.tar.gz" \
 && if [ -f "$TARBALL" ]; then \
      # hermetic path: verified against the published .sha256sum, so a corrupted or swapped file fails here rather than emitting a module the browser rejects
      echo "wasm-bindgen ${V}: using vendored ${TARBALL}" \
      && if [ -f "$TARBALL.sha256sum" ]; then \
           printf '%s  %s\n' "$(cut -d' ' -f1 "$TARBALL.sha256sum")" "$TARBALL" | sha256sum --check --status \
             || { echo "wasm-bindgen: vendored tarball does not match its .sha256sum" >&2; exit 1; }; \
           echo "wasm-bindgen ${V}: sha256 verified"; \
         fi; \
    else \
      # fallback only: build/vendor/ (populated by dev.sh, sha256-verified) is preferred; use the wasm-bindgen org URL directly (rustwasm/ 301s there); if it 504s, use the API asset endpoint (Accept: application/octet-stream, <id> from /releases/tags/${V})
      echo "wasm-bindgen ${V}: no vendored copy, downloading" \
      && curl -fsSL --retry 5 --retry-all-errors --retry-delay 3 --connect-timeout 20 \
           -o /tmp/wasm-bindgen.tgz \
           "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/${V}/wasm-bindgen-${V}-${WB}.tar.gz" \
      && TARBALL=/tmp/wasm-bindgen.tgz; \
    fi \
 && tar xzf "$TARBALL" -C /usr/local/bin --strip-components=1 \
      "wasm-bindgen-${V}-${WB}/wasm-bindgen" \
 && rm -f /tmp/wasm-bindgen.tgz \
 && test "$(wasm-bindgen --version | sed -n 's/[^0-9.]//gp')" = "$V" \
 && echo "wasm-bindgen ${V} (${WB}) matches the kit pin" \
 && cargo build --release --locked -p brain \
 && cargo build --release --locked -p brain-app --target wasm32-unknown-unknown \
 && wasm-bindgen --target web --out-dir web/pkg \
      target/wasm32-unknown-unknown/release/brain_app.wasm \
 && test -f web/pkg/brain_app.js && test -f web/pkg/brain_app_bg.wasm

# ── runtime ─────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Load-bearing, so nobody prunes it later: ca-certificates (TLS to the model gateway and to github.com for the data clone — both boot dependencies), git (the entrypoint clones the corpus), curl (HEALTHCHECK only).
COPY build/certs/ /usr/local/share/ca-certificates/
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git curl \
 && update-ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

# uid pinned rather than `useradd -r`: an auto-assigned uid shifts the day the base image adds a user, and 1000 is the conventional runAsUser so the image and the Deployment's securityContext agree.
RUN groupadd --gid 1000 brain \
 && useradd --uid 1000 --gid 1000 --no-create-home --shell /usr/sbin/nologin brain \
 && mkdir -p /opt/brain /data \
 && chown -R 1000:1000 /opt/brain /data

WORKDIR /opt/brain

# The release binary and the built frontend, and nothing else: no sources on disk (they would make the image look buildable, and the dev server's wasm_is_stale() reads those dirs) and no node_modules/ or target/ (see .containerignore).
COPY --from=build /src/target/release/brain ./brain
COPY --from=build /src/web ./web
# The kit's browser runtime (/app.mjs, hot.mjs, kit.css, phosphor); omit it and the page boots its shell, fetches /api/* fine, and never instantiates wasm.
# Copied to a sibling dir, not into web/, so the app root stays exactly what `git ls-files web/` says and the shadowing rule stays the server's.
COPY --from=build /src/vendor/rustex1/kit/web ./kit
COPY deploy/entrypoint.sh /usr/local/bin/brain-wiki-entrypoint

# The glibc guard: runs the binary the way the entrypoint will, because a build/runtime libc mismatch otherwise surfaces as a CrashLoopBackOff after a 30 s start-period.
RUN /opt/brain/brain --version \
 && /opt/brain/brain serve --help >/dev/null

EXPOSE 8080
USER brain

# /health answers without the database and without the model gateway on purpose: a pod with a slow neighbour is degraded, not dead, and conflating the two turns latency into a restart storm — readiness is the separate `ready` field.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/health >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/brain-wiki-entrypoint"]
