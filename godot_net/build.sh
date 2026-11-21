#!/bin/bash
#
# Builds the Godot client's network extension and copies it where the client loads it from
# (DyingStar: addons/ds_net/bin):
#   <out>/libds_godot_net.linux.x86_64.so
#   <out>/ds_godot_net.windows.x86_64.dll
#
# Usage: godot_net/build.sh <godot binary> <out dir> [linux|windows|all]
#
# The Godot binary must be the project's (4.7, precision=double): the bindings are generated
# from its API, dumped to godot/extension_api.json. Windows is cross-built from Linux with the
# x86_64-pc-windows-gnu target and MinGW-w64 (rustup target add x86_64-pc-windows-gnu; Fedora
# mingw64-gcc, Debian gcc-mingw-w64-x86-64).

set -euo pipefail

if [ $# -lt 2 ]; then
    echo "usage: $0 <godot binary> <out dir> [linux|windows|all]" >&2
    exit 2
fi
godot="$(realpath "$1")"
mkdir -p "$2"
out="$(realpath "$2")"
which="${3:-all}"

cd "$(dirname "$0")"
mkdir -p godot
(cd godot && "$godot" --headless --dump-extension-api > /dev/null)
if ! grep -q '"precision": "double"' godot/extension_api.json; then
    echo "$godot is not a double-precision build: the client would not load the extension" >&2
    exit 1
fi

if [ "$which" = linux ] || [ "$which" = all ]; then
    cargo build --release --locked
    cp target/release/libds_godot_net.so "$out/libds_godot_net.linux.x86_64.so"
fi
if [ "$which" = windows ] || [ "$which" = all ]; then
    export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER:-x86_64-w64-mingw32-gcc}"
    cargo build --release --locked --target x86_64-pc-windows-gnu
    cp target/x86_64-pc-windows-gnu/release/ds_godot_net.dll "$out/ds_godot_net.windows.x86_64.dll"
fi
ls -la "$out"
