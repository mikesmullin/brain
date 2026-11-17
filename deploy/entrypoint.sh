#!/bin/sh
# brain-wiki entrypoint — materialise the corpora, then serve them.
#
# The `.md` files under `db/` are the authoritative artifact and PostgreSQL is a
# rebuildable index over them (README: "the ONE .md -> index path" is `reindex`).
# So this container owns no state: it shallow-clones the corpora, derives the
# index, and serves. Losing the pod loses nothing but time.
#
# Kept in shell rather than baked into `brain serve` on purpose. Cloning a
# repository and rebuilding an index are deployment *policy* — what to clone,
# how often, whether to tolerate an unreachable origin. Inside the binary those
# choices would become invisible, and an app that quietly decides its own data
# provenance is an app you cannot reuse for another brain.
#
# Everything is overridable by environment, which is how a Deployment's `env`
# block configures it without a rebuild.
#
# ## Corpora are pulled, not baked
#
# The image carries no corpus. A corpus is a set of repositories described by
# `corpora.yaml` (mounted or passed as `BRAIN_CORPORA`), shallow-cloned here at
# boot into `BRAIN_CORPORA_DIR`. That keeps the image a function of ONE
# repository — brain — instead of two, so its tag is never a provenance lie, and
# it lets a corpus change land through `POST /api/corpus/refresh` instead of a
# rebuild-and-redeploy.
set -eu

# ── configuration ───────────────────────────────────────────────────────────
# The corpora file names the repositories, the refs, where the brain lives inside
# each repo (`subdir: .brain`) and which one a bare `/` lands on. It is the same
# file the running server reads for the refresh webhook, so boot and webhook can
# never disagree about the set.
: "${BRAIN_CORPORA:=/opt/brain/corpora.yaml}"
: "${BRAIN_CORPORA_DIR:=/data/corpora}"
: "${BRAIN_SERVE_HOST:=0.0.0.0}"
: "${BRAIN_SERVE_PORT:=8080}"

# What this invocation does. The default keeps `docker run` / `podman run` a
# one-command thing, but in Kubernetes the two halves belong in two places:
#
#   bootstrap  pull + reindex, then exit 0   → an initContainer
#   serve      exec the HTTP server          → the main container
#   both       the above, in sequence        → a plain container run
#
# Splitting them is not tidiness. With `both` in the main container, the process
# that owns the liveness target is not listening yet while it is cloning and
# indexing, so kubelet kills it at ~100 s (exit 137, "failed liveness probe",
# "connection refused" — measured on a local cluster 2026-11-08) and the pod
# restart-loops exactly as long as its own warm-up takes. An initContainer runs
# to completion before the server is even created, so the probe budget measures
# the server, not the corpora.
: "${BRAIN_ROLE:=both}"

log() { printf '%-9s %s\n' start "$*"; }
warn() { printf '%-9s %s\n' WARN "$*" >&2; }

# ── 1 · the corpora ─────────────────────────────────────────────────────────
bootstrap() {
  if [ ! -f "$BRAIN_CORPORA" ]; then
    warn "no corpora file at $BRAIN_CORPORA (mount one, or set BRAIN_CORPORA)"
    exit 1
  fi
  log "corpora $BRAIN_CORPORA -> $BRAIN_CORPORA_DIR"
  # `corpus pull` owns the failure policy, and it is the same policy the webhook
  # uses: a corpus that cannot be *cloned* is fatal (a wiki with no pages that
  # answers 200 is worse than a pod that says why), while a corpus that cannot be
  # *refreshed* keeps serving the checkout on disk, reports `stale`, and exits 0.
  #
  # It also reindexes whatever moved. On a fresh pod that is every corpus, into a
  # database that may not be up yet — in which case the reindex is skipped here
  # and done by the server, which opens the store with its own bounded retry and
  # rebuilds an empty index on the way in. Retrying PostgreSQL twice would be two
  # mechanisms that can disagree about readiness; this keeps one.
  if ! /opt/brain/brain corpus pull --file "$BRAIN_CORPORA" --dest "$BRAIN_CORPORA_DIR"; then
    # Exit non-zero -> Init:Error / CrashLoopBackOff, which names the problem.
    exit 1
  fi
  log "corpora ready at $BRAIN_CORPORA_DIR"
}

case "$BRAIN_ROLE" in
  bootstrap)
    bootstrap
    log "bootstrap complete"
    exit 0
    ;;
  serve)
    log "BRAIN_ROLE=serve; expecting corpora at $BRAIN_CORPORA_DIR"
    ;;
  both)
    bootstrap
    ;;
  *)
    warn "unknown BRAIN_ROLE='$BRAIN_ROLE' (want bootstrap|serve|both)"
    exit 2
    ;;
esac

# ── 2 · serve ───────────────────────────────────────────────────────────────
# exec, so brain is PID 1 and receives SIGTERM directly: the shell must not sit
# in front of it, or the graceful drain in serve.rs never happens and K8s waits
# out the full terminationGracePeriodSeconds on every rollout.
#
# `--corpora` also enables `POST /api/corpus/refresh`, which pulls the same file
# and swaps the brains in without a restart. It is authenticated by
# `BRAIN_WEBHOOK_SECRET` (bearer token or `X-Hub-Signature-256`); unset, the
# route refuses with 403 rather than exposing an open trigger.
log "brain serve on ${BRAIN_SERVE_HOST}:${BRAIN_SERVE_PORT}"
exec /opt/brain/brain serve \
  --corpora "$BRAIN_CORPORA" \
  --host "$BRAIN_SERVE_HOST" \
  --port "$BRAIN_SERVE_PORT" \
  --web /opt/brain/web \
  --assets /opt/brain/kit \
  --name brain-wiki
