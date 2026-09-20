#!/bin/sh
# Build the browser client into crates/b070-web/dist.
#
# The bridge serves whatever this produces:
#   b070-bridge --host <device> --static-dir crates/b070-web/dist
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/crates/b070-web/dist"
profile="${1:-release}"

case "$profile" in
  release) flags="--release"; target_dir="release" ;;
  debug)   flags="";          target_dir="debug"   ;;
  *) echo "usage: $0 [release|debug]" >&2; exit 2 ;;
esac

# wasm-bindgen must match the wasm-bindgen crate the build links against, or
# the generated glue and the module disagree about their ABI.
command -v wasm-bindgen >/dev/null 2>&1 || {
  echo "wasm-bindgen not found. Install the version matching Cargo.lock:" >&2
  echo "  cargo install wasm-bindgen-cli --version \
$(grep -A1 '^name = "wasm-bindgen"$' "$root/Cargo.lock" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)" >&2
  exit 1
}

echo "building b070-web ($profile)"
cargo build -p b070-web --target wasm32-unknown-unknown $flags

rm -rf "$out"
mkdir -p "$out"
wasm-bindgen \
  --target web \
  --no-typescript \
  --out-dir "$out" \
  "$root/target/wasm32-unknown-unknown/$target_dir/b070_web.wasm"

cp "$root/crates/b070-web/web/index.html" "$out/"

echo "wrote $out"
ls -la "$out"
