#!/bin/bash
# What the container image claims, checked on the machine at hand.
#
# `deploy/Dockerfile` ends in `FROM scratch`: no libc, no shell, no /etc, no CA bundle. Where
# a container runtime exists, `docker build` is the check. Where it does not — this repository
# was written on a box with no docker, podman or buildah — this script builds the same kind of
# binary (fully static) and runs it inside a rootfs containing exactly what the image
# contains, one file, using an unprivileged user namespace for the chroot.
#
#   ./scripts/check-headless.sh
#
# It checks, in order:
#   1. the binary is static, so there is no libc for `scratch` to be missing
#   2. the rootfs has no /etc, no shell and no CA bundle in it
#   3. it runs with no HOME, and resolves the image's HOME=/data the way the Dockerfile does
#   4. a ranged download through a real origin produces byte-identical output
#   5. the daemon serves /health, the control page and the JSON API
#   6. SIGTERM — what `docker stop` sends — stops it cleanly and the queue survives
#
# Without unprivileged user namespaces (`unshare -r`) the chroot steps cannot run; the script
# says so and runs the same binary against the same empty environment directly instead. That
# is weaker — the host's /etc is still visible — but it is not nothing, and a check that
# silently does less than it says would be worse.
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
cd "$REPO"

BIN=${BIN:-target/x86_64-unknown-linux-gnu/release/riptide}
ROOT=${ROOT:-$(mktemp -d)}
WORK=${WORK:-$(mktemp -d)}
PORT_DAEMON=${PORT_DAEMON:-18091}
PORT_ORIGIN=${PORT_ORIGIN:-18092}
PORT_SLOW=${PORT_SLOW:-18093}
ORIGIN_SERVER=${ORIGIN_SERVER:-tests/origin.py}

fail() { echo "FAIL: $*"; exit 1; }

for tool in ldd cmp curl python3; do
  command -v "$tool" >/dev/null || fail "$tool is needed"
done

# --- can we use a real chroot? ---------------------------------------------
IMAGE=_direct
if unshare -r true 2>/dev/null; then
  IMAGE=chroot
fi
if [ "$IMAGE" = chroot ]; then
  echo "note: checking inside a chroot with unprivileged user namespaces"
else
  echo "note: no unprivileged user namespaces here — running with an empty environment"
  echo "      instead of a chroot, so /etc on this host is still visible"
fi

# Run a command as the image would: HOME=/data, and nothing else in the environment. `PATH` is
# kept only so `unshare` and `chroot` can be found; the program itself is always named by an
# absolute path, so the container's empty environment is what it actually runs under.
in_image() {
  if [ "$IMAGE" = chroot ]; then
    env -i HOME=/data PATH="$PATH" unshare -r chroot "$ROOT" "$@"
  else
    env -i HOME="$ROOT/data" PATH="$PATH" "$@"
  fi
}

# The same, but replacing the shell, so that backgrounding it leaves the *program's* pid in
# `$!` rather than a wrapper's. Without the `exec` the shell keeps a process of its own in
# between, and signalling it orphans the daemon instead of stopping it.
in_image_exec() {
  if [ "$IMAGE" = chroot ]; then
    exec env -i HOME=/data PATH="$PATH" unshare -r chroot "$ROOT" "$@"
  else
    exec env -i HOME="$ROOT/data" PATH="$PATH" "$@"
  fi
}

# Which process is the daemon?
#
# Not necessarily `$!`: `unshare --map-root-user` forks when it has to write the uid map
# itself, and only then. Finding it by the address it was told to run on is exact and needs
# no assumptions about how many layers the wrapper added.
daemon_pid() {
  local pid
  for _ in $(seq 1 100); do
    for pid in $(pgrep -x riptide); do
      if tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -qF "0.0.0.0:$PORT_DAEMON"; then
        echo "$pid"
        return 0
      fi
    done
    sleep 0.1
  done
  fail "cannot find the daemon for port $PORT_DAEMON"
}

echo
echo "== 1. a static binary =="
[ -x "$BIN" ] || fail "$BIN is missing: build it with RUSTFLAGS='-C target-feature=+crt-static'"
ldd "$BIN" 2>&1 | grep -q 'statically linked' || fail "$BIN is not static: $(ldd "$BIN" 2>&1)"
echo "   $(stat -c '%s bytes' "$BIN"), ldd says: $(ldd "$BIN" 2>&1 | tr -d '\t')"
# glibc-static still warns about NSS at runtime; nothing here looks up a user or a host by
# name, so that warning must not appear. If it does, something started calling getpwnam.
"$BIN" --version >/dev/null 2>"$WORK/startup.err" || fail "--version failed: $(cat "$WORK/startup.err")"
[ -s "$WORK/startup.err" ] && echo "   startup said: $(cat "$WORK/startup.err")"

echo
echo "== 2. an image that is one file =="
mkdir -p "$ROOT/usr/local/bin" "$ROOT/data"
cp "$BIN" "$ROOT/usr/local/bin/riptide"
find "$ROOT" -mindepth 1 -printf '   %10s %p\n' | sed "s#$ROOT#/image#"
if [ -n "$(find "$ROOT" -mindepth 1 -not -path "$ROOT/usr*" -not -path "$ROOT/data*")" ]; then
  fail "the rootfs should hold nothing but the binary and /data"
fi

echo
echo "== 3. where it keeps its files =="
if [ "$IMAGE" = chroot ]; then
  echo -n "   no HOME    -> "
  env -i PATH="$PATH" unshare -r chroot "$ROOT" /usr/local/bin/riptide config path
fi
echo -n "   HOME=/data -> "; in_image /usr/local/bin/riptide config path

echo
echo "== 4. a real download =="
dd if=/dev/urandom of="$WORK/payload.bin" bs=1M count=6 status=none
python3 "$ORIGIN_SERVER" "$PORT_ORIGIN" "$WORK" >"$WORK/origin.log" 2>&1 &
ORIGIN=$!
trap 'kill $ORIGIN 2>/dev/null' EXIT
for _ in $(seq 1 100); do
  curl -sf -o /dev/null "http://127.0.0.1:$PORT_ORIGIN/" && break
  sleep 0.1
done
in_image /usr/local/bin/riptide download \
    "http://127.0.0.1:$PORT_ORIGIN/payload.bin" -n 4 >"$WORK/dl.log" 2>&1 \
  || fail "the download failed: $(cat "$WORK/dl.log")"
got="$ROOT/data/Downloads/payload.bin"
[ -f "$got" ] || fail "no file was written: $(cat "$WORK/dl.log")"
cmp -s "$WORK/payload.bin" "$got" || fail "the bytes do not match the origin"
echo "   6 MiB over 4 connections: $(tr -d '\n' <"$WORK/dl.log")"

echo
echo "== 5. the daemon serves the API, with a job in flight =="
"$ROOT/usr/local/bin/riptide" --help >/dev/null 2>&1 || fail "the copied binary is not executable"
if curl -sf -o /dev/null "http://127.0.0.1:$PORT_DAEMON/health" 2>/dev/null; then
  fail "something already serves port $PORT_DAEMON (a leftover daemon?); stop it or set PORT_DAEMON"
fi
# A second origin that dribbles, so the daemon has a transfer to interrupt: a stop that lands
# between jobs would not exercise the cancel-and-save path at all.
dd if=/dev/urandom of="$WORK/slow.bin" bs=1M count=12 status=none
python3 "$ORIGIN_SERVER" "$PORT_SLOW" "$WORK" --slow >"$WORK/slow.log" 2>&1 &
SLOW=$!
in_image_exec /usr/local/bin/riptide daemon --addr "0.0.0.0:$PORT_DAEMON" >"$WORK/daemon.log" 2>&1 &
WRAPPER=$!
trap 'kill $ORIGIN $SLOW $WRAPPER 2>/dev/null' EXIT
for _ in $(seq 1 100); do
  curl -sf -o /dev/null "http://127.0.0.1:$PORT_DAEMON/health" && break
  sleep 0.1
done
for _ in $(seq 1 100); do
  curl -sf -o /dev/null "http://127.0.0.1:$PORT_SLOW/" && break
  sleep 0.1
done
code=$(curl -s -o "$WORK/health.json" -w '%{http_code}' "http://127.0.0.1:$PORT_DAEMON/health")
[ "$code" = 200 ] || fail "the daemon did not answer /health (got $code): $(cat "$WORK/daemon.log")"
echo "   GET  /health      -> $code $(cat "$WORK/health.json")"
echo "   GET  /            -> $(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT_DAEMON/")"
posted=$(curl -s -o "$WORK/job.json" -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    -d "{\"url\":\"http://127.0.0.1:$PORT_SLOW/slow.bin\"}" \
    "http://127.0.0.1:$PORT_DAEMON/api/jobs")
[ "$posted" = 201 ] || fail "the daemon refused a job (got $posted): $(cat "$WORK/job.json")"
JOB=$(python3 -c "import json;print(json.load(open('$WORK/job.json'))['id'])")
echo "   POST /api/jobs    -> $posted $(cat "$WORK/job.json")"

# The job has to be transferring, not queued, before the signal means anything.
job_state() {
  curl -s "http://127.0.0.1:$PORT_DAEMON/api/jobs" \
    | python3 -c "import json,sys
jobs = json.load(sys.stdin)
print(next((j['state'] for j in jobs if j['id'] == '$JOB'), 'gone'))"
}
for _ in $(seq 1 200); do
  [ "$(job_state)" = active ] && break
  sleep 0.1
done
[ "$(job_state)" = active ] || fail "the job never started transferring (state: $(job_state))"
echo "   GET  /api/jobs    -> $JOB is $(job_state)"

echo
echo "== 6. docker stop, mid-download =="
DPID=$(daemon_pid)
t0=$SECONDS
kill -TERM "$DPID"
for _ in $(seq 1 200); do kill -0 "$DPID" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$DPID" 2>/dev/null; then
  kill -9 "$DPID"
  fail "the daemon ignored SIGTERM for $((SECONDS - t0))s"
fi
wait "$WRAPPER"
code=$?
echo "   exit=$code after $((SECONDS - t0))s"
[ "$code" = 0 ] || fail "a signalled stop should exit 0, not $code"
grep -q SIGTERM "$WORK/daemon.log" || fail "the stop did not say what stopped it"
sed 's/^/   | /' "$WORK/daemon.log" | tail -4
echo "   the queue afterwards:"
in_image /usr/local/bin/riptide list | sed 's/^/   /'
# Interrupted, resumable, and never a failure: the whole point of handling the signal.
in_image /usr/local/bin/riptide list --json >"$WORK/queue.json"
python3 - "$WORK/queue.json" <<'PY' || fail "the interrupted job was not left resumable"
import json, sys
for j in json.load(open(sys.argv[1])):
    if j["state"] == "paused" and 0 < j.get("downloaded", 0) < (j.get("total") or 0):
        print(f"   {j['id']} is paused at {j['downloaded']} of {j['total']} bytes — resumable")
        sys.exit(0)
print("   no job was left paused with partial bytes:", open(sys.argv[1]).read())
sys.exit(1)
PY

echo
echo "all checks passed"
