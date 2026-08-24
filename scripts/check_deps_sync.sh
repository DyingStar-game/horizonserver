#!/bin/bash
#
# The ds_* plugins are a cargo workspace separate from Horizon's, so the shared
# crate versions in our [workspace.dependencies] are a hand-maintained copy of
# Horizon's. When they drift, the build still succeeds and the plugins blow up
# at runtime instead — a tokio mismatch means a runtime handle made by the host
# is invalid inside a plugin dylib.
#
# Run this after every submodule bump. See docs/horizon-fork.md.

set -euo pipefail

cd "$(dirname "$0")/.."

if [ ! -f Horizon/Cargo.toml ]; then
    echo "Horizon/Cargo.toml is missing — run scripts/install.sh first." >&2
    exit 1
fi

exec python3 - Cargo.toml Horizon/Cargo.toml <<'PY'
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
