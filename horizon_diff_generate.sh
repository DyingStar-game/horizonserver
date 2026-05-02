#!/usr/bin/env bash
# Generate a patch between the upstream Horizon source and the local
# modified Horizon folder. The resulting horizon.diff can be applied with:
#   cd /path/to/Horizon-clone && git apply --ignore-whitespace /path/to/horizon.diff
set -euo pipefail

SRC="/datas/Temp/Horizon"
DST="$(cd "$(dirname "$0")" && pwd)/Horizon"
OUT="$(cd "$(dirname "$0")" && pwd)/horizon.diff"

# Run diff from a temp dir with symlinks named "a" and "b" so the patch
# headers look like "a/Cargo.toml" / "b/Cargo.toml". git apply -p1
# (the default) then strips the single "a/" or "b/" prefix and finds
# the files correctly in the upstream clone — regardless of the absolute
# paths of SRC and DST on this machine.
#
# --strip-trailing-cr normalises CRLF→LF in both source files before
# comparing, so the generated patch has consistent LF context lines and
# applies cleanly even when the upstream repo ships CRLF files.
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

ln -s "$SRC" "$TMPDIR/a"
ln -s "$DST" "$TMPDIR/b"

cd "$TMPDIR"
diff -urN \
    --strip-trailing-cr \
    --exclude=target \
    --exclude=.git \
    --exclude=logs \
    --exclude='*.sock' \
    a b > "$OUT" || true

echo "Wrote $OUT"
