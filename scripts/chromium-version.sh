#!/bin/sh
# Prints the Chromium version the engine bindings were built against, from the name of the
# distribution archive cargo unpacked (`..._chromium-156.0.8078.100_macosarm64_...`).
set -e
cd "$(dirname "$0")/.."
f=$(ls -t target/release/build/cef-dll-sys-*/out/cef_binary_*.tar.bz2 target/debug/build/cef-dll-sys-*/out/cef_binary_*.tar.bz2 2>/dev/null | head -n 1)
[ -n "$f" ] || { echo "chromium-version: no engine distribution under target/" >&2; exit 1; }
printf '%s\n' "$f" | sed -n 's/.*chromium-\([0-9.]*\)_.*/\1/p'
