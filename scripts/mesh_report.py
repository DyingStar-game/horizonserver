#!/usr/bin/env python3
"""Turns a capture of scripts/mesh_watch.sh into numbers and charts.

Reads the `[mesh] state {json}` lines (every 5 s) and the `[mesh] op started/done`
lines of horizon.log and writes, next to it:
  servers.csv   one row per server per sample (players as Godot / Horizon count them, tps)
  zones.csv     one row per zone each time a server's zones change (bounds + size)
  ops.csv       mesh transitions (split, merge, grant...) with their duration
  mesh.png      players / tps per server over time, ops as vertical lines
and prints the anomalies: servers above the split rule, Godot and Horizon
disagreeing, players missing from every server.

usage: python3 scripts/mesh_report.py <capture_dir> [--split 75] [--expected 300]
"""
import argparse
import csv
import json
import re
import sys
from collections import defaultdict
from datetime import datetime
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")
TS = re.compile(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z")


def ts(line):
    m = TS.search(line)
    return datetime.fromisoformat(m.group(1)[:26]) if m else None


def zone_label(z):
    world = "space" if z.get("world") == "space" else "planet:%s" % z.get("planet_name")
    return world


def zone_size(b):
    if not b:
        return None
    big = 1e11  # an edge at the unbounded extent
    return tuple(None if abs(b["min_" + a]) >= big or abs(b["max_" + a]) >= big else b["max_" + a] - b["min_" + a] for a in "xyz")


def fmt(v):
    return "inf" if v is None else "%.0f" % v


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--split", type=int, default=75, help="players of the split rule")
    ap.add_argument("--expected", type=int, default=0, help="players spawned (to spot lost ones)")
    args = ap.parse_args()
    out = Path(args.dir)

    states, ops = [], []
    open_ops = {}
    for raw in (out / "horizon.log").open(errors="replace"):
        line = ANSI.sub("", raw)
        if "[mesh] state " in line:
            t = ts(line)
            try:
                states.append((t, json.loads(line.split("[mesh] state ", 1)[1])))
            except json.JSONDecodeError:
                pass
        elif "[mesh] op started: " in line:
            label = line.split("[mesh] op started: ", 1)[1].strip()
            open_ops[label] = ts(line)
        elif "[mesh] op done: " in line:
            rest = line.split("[mesh] op done: ", 1)[1].strip()
            label, _, took = rest.rpartition(" after ")
            ops.append((open_ops.pop(label, None), ts(line), label, took))
    if not states:
        sys.exit("no `[mesh] state` line in %s/horizon.log (is the new ds_game_server plugin deployed?)" % out)
    t0 = states[0][0]
    rel = lambda t: (t - t0).total_seconds() if t else None

    # ---- servers.csv / zones.csv
    series = defaultdict(lambda: {"t": [], "godot": [], "horizon": [], "tps": [], "pending": [], "outbox": []})
    last_zones = {}
    zone_rows = []
    with (out / "servers.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t_s", "time", "server", "state", "tps", "players_godot", "players_horizon", "pending", "split_hits", "settling", "zones"])
        for t, s in states:
            for srv in s["servers"]:
                zones = srv.get("zones") or []
                labels = []
                for z in zones:
                    size = zone_size(z.get("bounds"))
                    labels.append(zone_label(z) + ("" if size is None else "[%s]" % "x".join(fmt(v) for v in size)))
                w.writerow([rel(t), t.isoformat(), srv["name"], srv["state"], srv["tps"], srv["players_godot"],
                            srv["players_horizon"], srv.get("pending"), srv["split_hits"], srv["settling"], " ".join(labels)])
                if srv["state"] == "Running":
                    d = series[srv["name"]]
                    d["t"].append(rel(t))
                    d["godot"].append(srv["players_godot"] or 0)
                    d["horizon"].append(srv["players_horizon"])
                    d["tps"].append(srv["tps"] or 0)
                    d["pending"].append(srv.get("pending") or 0)
                    d["outbox"].append(srv.get("outbox") or 0)
                key = json.dumps(zones, sort_keys=True)
                if last_zones.get(srv["name"]) != key:
                    last_zones[srv["name"]] = key
                    for z in zones or [None]:
                        b = (z or {}).get("bounds")
                        size = zone_size(b)
                        zone_rows.append([rel(t), t.isoformat(), srv["name"], srv["state"],
                                          zone_label(z) if z else "-", (z or {}).get("id", ""),
                                          *([b["min_" + a] for a in "xyz"] + [b["max_" + a] for a in "xyz"] if b else [""] * 6),
                                          *([fmt(v) for v in size] if size else ["whole world"] * 3)])
    with (out / "zones.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t_s", "time", "server", "state", "world", "zone_id", "min_x", "min_y", "min_z", "max_x", "max_y", "max_z", "size_x", "size_y", "size_z"])
        w.writerows(zone_rows)
    with (out / "ops.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["start_s", "done_s", "took", "op"])
        for start, done, label, took in ops:
            w.writerow([rel(start), rel(done), took, label])

    # ---- anomalies
    print("samples: %d over %.0f s, %d mesh ops" % (len(states), rel(states[-1][0]), len(ops)))
    print("\npeak players per server (godot / horizon), samples above split (%d):" % args.split)
    for name, d in sorted(series.items()):
        over = sum(1 for g in d["godot"] if g > args.split)
        print("  %-16s godot max %4d  horizon max %4d  over-split %3d samples (%.0f s)  min tps %s  max queued in/out %d/%d" % (
            name, max(d["godot"]), max(d["horizon"]), over, over * 5, min(d["tps"]), max(d["pending"]), max(d["outbox"])))
    print("\nlongest runs above the split rule:")
    for name, d in sorted(series.items()):
        run, best, start = 0, (0, None), None
        for t, g in zip(d["t"], d["godot"]):
            if g > args.split:
                start = t if run == 0 else start
                run += 1
                best = max(best, (run, start), key=lambda x: x[0])
            else:
                run = 0
        if best[0] >= 6:
            print("  %-16s %3d samples from t=%.0f s" % (name, best[0], best[1]))
    print("\nGodot vs Horizon disagreement > 5 players (first 20):")
    n = 0
    for t, s in states:
        for srv in s["servers"]:
            g, h = srv["players_godot"] or 0, srv["players_horizon"]
            if srv["state"] == "Running" and abs(g - h) > 5 and n < 20:
                print("  t=%5.0f %-16s godot %4d horizon %4d settling=%s" % (rel(t), srv["name"], g, h, srv["settling"]))
                n += 1
    print("\ntotal players over time (godot / horizon):")
    step = max(1, len(states) // 25)
    for t, s in states[::step]:
        run = [x for x in s["servers"] if x["state"] == "Running"]
        tg = sum(x["players_godot"] or 0 for x in run)
        th = sum(x["players_horizon"] for x in run)
        flag = "  <- missing %d" % (args.expected - th) if args.expected and th < args.expected - 5 else ""
        print("  t=%5.0f  %2d running  godot %4d  horizon %4d  in_flight=%s%s" % (rel(t), len(run), tg, th, s["in_flight"], flag))
    print("\nfinal zones:")
    for srv in states[-1][1]["servers"]:
        if srv["state"] != "Online":
            print("  %-16s %-8s %s" % (srv["name"], srv["state"], " ".join(
                zone_label(z) + ("" if not z.get("bounds") else "[%s]" % "x".join(fmt(v) for v in zone_size(z["bounds"])))
                for z in srv["zones"])))

    # ---- chart
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("\n(matplotlib missing, no chart)")
        return
    fig, (a1, a2, a3) = plt.subplots(3, 1, figsize=(15, 11), sharex=True)
    for name, d in sorted(series.items()):
        (l,) = a1.plot(d["t"], d["godot"], label=name)
        a2.plot(d["t"], d["horizon"], color=l.get_color())
        a3.plot(d["t"], d["tps"], color=l.get_color())
    for ax in (a1, a2):
        ax.axhline(args.split, color="red", ls="--", lw=1)
    for start, done, label, _ in ops:
        kind = label.split()[0]
        if kind in ("split", "merge") and start is not None:
            for ax in (a1, a2, a3):
                ax.axvline(rel(start), color="green" if kind == "split" else "purple", lw=0.8, alpha=0.6)
    a1.set_ylabel("players (Godot)")
    a2.set_ylabel("players (Horizon)")
    a3.set_ylabel("tps")
    a3.set_xlabel("s since first sample  (green = split, purple = merge)")
    a1.legend(fontsize=7, ncol=5)
    fig.tight_layout()
    fig.savefig(out / "mesh.png", dpi=110)
    print("\nwrote %s/{servers,zones,ops}.csv and mesh.png" % out)


if __name__ == "__main__":
    main()
