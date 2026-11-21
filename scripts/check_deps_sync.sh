#!/bin/bash
#
# The ds_* plugins are a cargo workspace separate from Horizon's, so the shared
# crate versions in our [workspace.dependencies] are a hand-maintained copy of
# Horizon's. When they drift, the build still succeeds and the plugins blow up
# at runtime instead — a tokio mismatch means a runtime handle made by the host
# is invalid inside a plugin dylib.
#
# Requirements agreeing is not enough either: each workspace resolves its own versions.
# The server builds with the plugins' Cargo.lock (.docker/Dockerfile.server), so the
# second check resolves it that way and compares the versions actually linked. A crate
# whose types cross the dylib boundary must be the very same version on both sides:
# with bytes 1.12.1 in the server and 1.11.1 in the plugins, Horizon built fine and
# segfaulted on its first WebTransport client (bytes::Bytes in Outbound).
#
# Run this after every submodule bump. See docs/horizon-fork.md.

set -euo pipefail

cd "$(dirname "$0")/.."

if [ ! -f Horizon/Cargo.toml ]; then
    echo "Horizon/Cargo.toml is missing — run scripts/install.sh first." >&2
    exit 1
fi

python3 - Cargo.toml Horizon/Cargo.toml <<'PY'
import sys, tomllib

def deps(path):
    with open(path, "rb") as fh:
        return tomllib.load(fh).get("workspace", {}).get("dependencies", {})

ours_path, theirs_path = sys.argv[1], sys.argv[2]
ours, theirs = deps(ours_path), deps(theirs_path)

def identity(spec):
    """The part that has to match: version, or the exact git revision."""
    if isinstance(spec, str):
        return ("version", spec)
    if "git" in spec:
        return ("git", spec["git"], spec.get("rev") or spec.get("branch") or spec.get("tag"))
    if "path" in spec:
        return None                      # local paths are ours by definition
    return ("version", spec.get("version"))

def features(spec):
    return sorted(spec.get("features", [])) if isinstance(spec, dict) else []

shared = sorted(set(ours) & set(theirs))
errors, warnings = [], []

for name in shared:
    a, b = identity(ours[name]), identity(theirs[name])
    if a is None or b is None:
        continue
    if a != b:
        errors.append(f"  {name}: {ours_path} has {a[1:]!r}, {theirs_path} has {b[1:]!r}")
    elif features(ours[name]) != features(theirs[name]):
        warnings.append(
            f"  {name}: features {features(ours[name])} vs {features(theirs[name])}"
        )

for name in sorted(set(ours) - set(theirs)):
    if identity(ours[name]) is not None:
        warnings.append(f"  {name}: declared here only — ours, or dropped upstream")

if warnings:
    print(f"Warnings ({len(warnings)}):")
    print("\n".join(warnings))

if errors:
    print(f"\nVersion mismatch between {ours_path} and {theirs_path} ({len(errors)}):")
    print("\n".join(errors))
    print("\nAlign them, then rebuild both the plugins and the server.")
    sys.exit(1)

print(f"\nOK: {len(shared)} shared dependencies agree.")
PY

# Resolved versions: the server's lock as its build makes it (the plugins' lock, plus
# the crates only the server uses), compared with the plugins'.
cp Cargo.lock Horizon/Cargo.lock
cargo metadata --format-version 1 --manifest-path Horizon/Cargo.toml > /dev/null

python3 - Cargo.lock Horizon/Cargo.lock <<'PY'
import sys, tomllib

def versions(path):
    """name -> {compatibility line -> version}, registry and git crates only."""
    out = {}
    with open(path, "rb") as fh:
        for pkg in tomllib.load(fh).get("package", []):
            if "source" not in pkg:
                continue                 # workspace members and path crates
            major, minor, patch = (pkg["version"].split("+")[0].split("-")[0].split(".") + ["0", "0"])[:3]
            line = major if major != "0" else (f"0.{minor}" if minor != "0" else f"0.0.{patch}")
            out.setdefault(pkg["name"], {})[line] = pkg["version"]
    return out

plugins_path, server_path = sys.argv[1], sys.argv[2]
plugins, server = versions(plugins_path), versions(server_path)
errors = []
for name in sorted(set(plugins) & set(server)):
    for line in sorted(set(plugins[name]) & set(server[name])):
        if plugins[name][line] != server[name][line]:
            errors.append(f"  {name}: plugins {plugins[name][line]}, server {server[name][line]}")

if errors:
    print(f"\nThe server resolves other versions than the plugins ({len(errors)}):")
    print("\n".join(errors))
    print("\nThe server needs a newer version than the plugins' lock holds: run")
    print("`cargo update -p <crate>` in this workspace, commit Cargo.lock, rebuild both.")
    sys.exit(1)

shared = sum(len(set(plugins[n]) & set(server.get(n, {}))) for n in plugins)
print(f"OK: the server links the plugins' version of all {shared} shared crates.")
PY
