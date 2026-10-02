#!/usr/bin/env bash
# Captures what a server-meshing load test needs, from the start of the test:
#   - the full horizon log (it rotates in minutes under load),
#   - `kubectl top pods` every 10 s,
#   - the log of every godotserver pod, without its per-frame noise.
# Stop with Ctrl-C, then: python3 scripts/mesh_report.py <out_dir>
#
# usage: scripts/mesh_watch.sh [out_dir]   (KCTX / KNS override context / namespace)
set -euo pipefail

KCTX=${KCTX:-minikube}
KNS=${KNS:-dyingstar}
OUT=${1:-mesh-test-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$OUT/godot"
k() { kubectl --context "$KCTX" -n "$KNS" "$@"; }

# 11 `kubectl logs -f` exhaust minikube's inotify instances ("failed to create
# fsnotify watcher: too many open files") and the horizon stream stops silently.
if [ "$KCTX" = minikube ]; then
    minikube ssh -- "sudo sysctl -w fs.inotify.max_user_instances=1024" >/dev/null 2>&1 || true
fi

pids=()
# The pod keeps its whole log until it restarts: take a full copy at the end, in
# case the stream died along the way.
cleanup() {
    kill "${pids[@]}" 2>/dev/null || true
    k logs --timestamps -c horizon "$horizon" > "$OUT/horizon.full.log" 2>/dev/null \
        && [ "$(wc -l < "$OUT/horizon.full.log")" -gt "$(wc -l < "$OUT/horizon.log")" ] \
        && mv "$OUT/horizon.full.log" "$OUT/horizon.log"
    rm -f "$OUT/horizon.full.log"
    echo; echo "captured in $OUT"
}
trap cleanup EXIT INT TERM

horizon=$(k get pods -l app=horizon -o name 2>/dev/null | head -1)
[ -n "$horizon" ] || horizon=$(k get pods -o name | grep '/horizon-' | head -1)
echo "horizon: $horizon"
k logs -f --timestamps -c horizon "$horizon" > "$OUT/horizon.log" &
pids+=($!)

for pod in $(k get pods -o name | grep '/godotserver-'); do
    name=${pod#pod/}
    k logs -f --timestamps "$pod" 2>&1 \
        | grep --line-buffered -vE 'Freeze object|loaded chunk' > "$OUT/godot/$name.log" &
    pids+=($!)
done

(while true; do
    echo "== $(date -u +%FT%TZ)"
    k top pods --no-headers 2>/dev/null || true
    sleep 10
done) > "$OUT/top.log" &
pids+=($!)

echo "capturing into $OUT (Ctrl-C to stop)"
while true; do
    sleep 15
    line=$(grep -a '\[mesh\] state' "$OUT/horizon.log" | tail -1 | sed 's/.*\[mesh\] state //')
    [ -n "$line" ] && python3 -c '
import json, sys
s = json.loads(sys.argv[1])
run = [x for x in s["servers"] if x["state"] == "Running"]
print("%s running, players %s, in_flight=%s" % (len(run),
      " ".join("%s:%s/%s" % (x["name"], x["players_godot"], x["players_horizon"]) for x in run), s["in_flight"]))
' "$line" || true
done
