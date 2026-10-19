#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-metaharness.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-metaharness" ;;
    *) rel="x86_64-apple-darwin/gray-metaharness" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-metaharness" ;;
    *) rel="x86_64-unknown-linux-musl/gray-metaharness" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-metaharness.exe" "$here/target/release/gray-metaharness" "$here/target/debug/gray-metaharness.exe" "$here/target/debug/gray-metaharness"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-metaharness binary not found in $here" >&2
exit 1
