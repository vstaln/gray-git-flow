#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/git-flow.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/git-flow" ;;
    *) rel="x86_64-apple-darwin/git-flow" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/git-flow" ;;
    *) rel="x86_64-unknown-linux-musl/git-flow" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/git-flow.exe" "$here/target/release/git-flow" "$here/target/debug/git-flow.exe" "$here/target/debug/git-flow"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "git-flow binary not found in $here" >&2
exit 1
