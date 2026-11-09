#!/bin/sh
# brain-wiki entrypoint — materialise the corpus, index it, serve it.
#
# The `.md` files under db/ are the authoritative artifact and PostgreSQL is a
# rebuildable index over them (README: "the ONE .md -> index path" is `reindex`).
# So this container owns no state: it clones the corpus, derives the index, and
# serves. Losing the pod loses nothing but time.
#
# Kept in shell rather than baked into `brain serve` on purpose. Cloning a repo
# and rebuilding an index are deployment policy — what to clone, how often,
# whether to tolerate an unreachable origin. Inside the binary those choices
# would become invisible, and an app that quietly decides its own data provenance
# is an app you cannot reuse for another brain.
#
# Everything is overridable by environment, which is how a Deployment's `env`
# block configures it without a rebuild.
set -eu

# ── configuration ───────────────────────────────────────────────────────────
# Public, unauthenticated, and deliberately so: this is the only credential-free
# network dependency in the boot path. When the brain learns to commit back to
# the git host, that write path needs a secret and this block will need a sibling —
# do not solve it by making this one use a token.
: "${BRAIN_DATA_REPO:=https://github.com/mikesmullin/brain-db-dstwiki.git}"
: "${BRAIN_DATA_REF:=main}"
: "${BRAIN_DATA_DIR:=/data/dstwiki}"
: "${BRAIN_SERVE_HOST:=0.0.0.0}"
: "${BRAIN_SERVE_PORT:=8080}"

# always | once | never
#   always  reindex on every boot. Correct by construction — no stale-index
#           class of bug exists — and cheap while brain.yaml sets `embed:
#           model: none` (keyword + graph only, no provider round-trips).
#   once    reindex only if this pod has never done it (marker in the emptyDir).
#           Switch to this the day embeddings are enabled: dstwiki measured
#           ~103 s of embedding calls, which is a long time to hold a probe open.
#   never   serve whatever the schema already holds. Debugging an index that
#           keeps getting rebuilt underneath you.
: "${BRAIN_REINDEX:=always}"

# Bound on waiting for PostgreSQL. The server binds its socket before it opens
# the database (`serve.rs` "Why the socket binds before the database does"), so
# exceeding this is a loud exit rather than a silent 503 forever.
: "${BRAIN_PG_WAIT_SEC:=180}"

# What this invocation does. The default keeps `docker run` / `podman run` a
# one-command thing, but in Kubernetes the two halves belong in two places:
#
#   bootstrap  clone + reindex, then exit 0   → an initContainer
#   serve      exec the HTTP server            → the main container
#   both       the above, in sequence          → a plain container run
#
# Splitting them is not tidiness. With `both` in the main container, the process
# that owns the liveness target is not listening yet while it is cloning and
# indexing, so kubelet kills it at ~100 s (exit 137, "failed liveness probe",
# "connection refused" — measured on a local cluster 2026-11-08) and the pod restart-
# loops exactly as long as its own warm-up takes. An initContainer runs to
# completion before the server is even created, so the probe budget measures the
# server, not the corpus.
: "${BRAIN_ROLE:=both}"

log() { printf '%-9s %s\n' start "$*"; }
warn() { printf '%-9s %s\n' WARN "$*" >&2; }

# ── 1 & 2 · corpus and index, when this invocation is asked to prepare them ──
bootstrap() {
# ── corpus ──────────────────────────────────────────────────────────────
#
# A fresh clone is fatal if it fails — a wiki with no pages that answers 200 is
# worse than a pod that crash-loops and says why. An *update* failure is not:
# falling back to the corpus already on disk is strictly better than no service,
# and the log line makes the staleness visible.
if [ -d "$BRAIN_DATA_DIR/db" ]; then
  log "corpus present at $BRAIN_DATA_DIR; refreshing $BRAIN_DATA_REF"
  if git -C "$BRAIN_DATA_DIR" fetch --depth 1 origin "$BRAIN_DATA_REF" \
     && git -C "$BRAIN_DATA_DIR" reset --hard FETCH_HEAD; then
    log "corpus refreshed to $(git -C "$BRAIN_DATA_DIR" rev-parse --short HEAD)"
  else
    warn "refresh failed; serving the corpus already on disk (may be stale)"
  fi
else
  log "cloning $BRAIN_DATA_REPO@$BRAIN_DATA_REF -> $BRAIN_DATA_DIR"
  tmp="$(mktemp -d)"
  # Clone to a temp dir and move it in, so an interrupted clone never leaves a
  # half-populated db/ that the next boot mistakes for a corpus.
  trap 'rm -rf "$tmp"' EXIT
  git clone --depth 1 --branch "$BRAIN_DATA_REF" "$BRAIN_DATA_REPO" "$tmp"
  if [ ! -d "$tmp/db" ]; then
    warn "$BRAIN_DATA_REPO has no db/ at $BRAIN_DATA_REF — not a brain repository"
    exit 1
  fi
  mkdir -p "$(dirname "$BRAIN_DATA_DIR")"
  rm -rf "$BRAIN_DATA_DIR"
  mv "$tmp" "$BRAIN_DATA_DIR"
  trap - EXIT
  log "cloned at $(git -C "$BRAIN_DATA_DIR" rev-parse --short HEAD); \
$(find "$BRAIN_DATA_DIR/db" -name '*.md' | wc -l | tr -d ' ') entity files"
fi

# ── 2 · index ───────────────────────────────────────────────────────────────
marker="$BRAIN_DATA_DIR/.brain-indexed"

do_reindex() {
  # `reindex` opens the store, so it doubles as the readiness probe for
  # PostgreSQL: retrying it IS the wait, and there is no second mechanism that
  # could disagree with the first about whether the database is up.
  waited=0
  delay=2
  while :; do
    if /opt/brain/brain --db "$BRAIN_DATA_DIR" reindex; then
      : > "$marker"
      log "index rebuilt from $BRAIN_DATA_DIR/db"
      return 0
    fi
    if [ "$waited" -ge "$BRAIN_PG_WAIT_SEC" ]; then
      return 1
    fi
    log "reindex failed (PostgreSQL not ready?); retrying in ${delay}s (${waited}s / ${BRAIN_PG_WAIT_SEC}s)"
    sleep "$delay"
    waited=$((waited + delay))
    delay=$((delay * 2)); [ "$delay" -gt 16 ] && delay=16
  done
}

case "$BRAIN_REINDEX" in
  never)
    log "BRAIN_REINDEX=never; serving the schema as-is"
    ;;
  once)
    if [ -f "$marker" ]; then
      log "BRAIN_REINDEX=once and this pod already indexed; skipping"
    elif ! do_reindex; then
      warn "no PostgreSQL after ${BRAIN_PG_WAIT_SEC}s; /api/* will 503 until it answers"
    fi
    ;;
  always)
    if ! do_reindex; then
      warn "no PostgreSQL after ${BRAIN_PG_WAIT_SEC}s; /api/* will 503 until it answers"
    fi
    ;;
  *)
    warn "unknown BRAIN_REINDEX='$BRAIN_REINDEX' (want always|once|never)"
    exit 2
    ;;
esac
}

case "$BRAIN_ROLE" in
  bootstrap)
    # Exit 0 and let the pod proceed to the server. A failure here is a non-zero
    # exit by `set -e`, which surfaces as Init:Error / CrashLoopBackOff on the
    # init container — a clearer signal than a server that is up and empty.
    bootstrap
    log "bootstrap complete"
    exit 0
    ;;
  serve)
    log "BRAIN_ROLE=serve; expecting the corpus at $BRAIN_DATA_DIR"
    ;;
  both)
    bootstrap
    ;;
  *)
    warn "unknown BRAIN_ROLE='$BRAIN_ROLE' (want bootstrap|serve|both)"
    exit 2
    ;;
esac

# ── 3 · serve ───────────────────────────────────────────────────────────────
# exec, so brain is PID 1 and receives SIGTERM directly: the shell must not sit
# in front of it, or the graceful drain in serve.rs never happens and K8s waits
# out the full terminationGracePeriodSeconds on every rollout.
log "brain serve on ${BRAIN_SERVE_HOST}:${BRAIN_SERVE_PORT}"
exec /opt/brain/brain serve \
  --db "$BRAIN_DATA_DIR" \
  --host "$BRAIN_SERVE_HOST" \
  --port "$BRAIN_SERVE_PORT" \
  --web /opt/brain/web \
  --assets /opt/brain/kit \
  --name brain-wiki
