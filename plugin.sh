#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-telegram.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-telegram" ;;
    *) rel="x86_64-apple-darwin/gray-telegram" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-telegram" ;;
    *) rel="x86_64-unknown-linux-musl/gray-telegram" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-telegram.exe" "$here/target/release/gray-telegram" "$here/target/debug/gray-telegram.exe" "$here/target/debug/gray-telegram"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-telegram binary not found in $here" >&2
exit 1
