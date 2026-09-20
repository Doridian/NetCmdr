#!/bin/sh
# Build the browser client into crates/netcmdr-web/dist.
#
# netcmdr-bridge embeds whatever this produces, so building the client is the
# first of three steps:
#   tools/build-web.sh
#   tools/pack-web.sh
#   go build ./bridge
# and the resulting binary is the whole service — there is nothing to deploy
# beside it. `nix build .#netcmdr-bridge` does all three.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/crates/netcmdr-web/dist"
profile="${1:-release}"

case "$profile" in
  release) flags="--profile web"; target_dir="web"   ;;
  debug)   flags="";              target_dir="debug" ;;
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

echo "building netcmdr-web ($profile)"
cargo build -p netcmdr-web --target wasm32-unknown-unknown $flags

rm -rf "$out"
mkdir -p "$out"
wasm-bindgen \
  --target web \
  --no-typescript \
  --out-dir "$out" \
  "$root/target/wasm32-unknown-unknown/$target_dir/netcmdr_web.wasm"

cp "$root/crates/netcmdr-web/web/index.html" "$out/"

# wasm-opt is optional: it roughly halves the module, but a build without it
# still works, so a missing binaryen is a note rather than a failure.
wasm="$out/netcmdr_web_bg.wasm"
if [ "$profile" = release ] && command -v wasm-opt >/dev/null 2>&1; then
  before=$(wc -c < "$wasm")
  # -Oz is size over speed, which is the right trade for a module the browser
  # downloads once and then runs a video decoder in. The feature flags are the
  # ones rustc emits by default; older binaryen refuses the module without them.
  wasm-opt -Oz \
    --enable-bulk-memory \
    --enable-nontrapping-float-to-int \
    --enable-sign-ext \
    --enable-mutable-globals \
    --enable-reference-types \
    --enable-multivalue \
    -o "$wasm.opt" "$wasm"
  mv "$wasm.opt" "$wasm"
  after=$(wc -c < "$wasm")
  echo "wasm-opt: $((before / 1024)) KiB -> $((after / 1024)) KiB"
else
  if [ "$profile" = release ]; then
    echo "wasm-opt not found; shipping the unoptimised module." >&2
    echo "  install it with:  sudo pacman -S binaryen   (or your package manager's equivalent)" >&2
  fi
fi

# Nothing is compressed here. tools/pack-web.sh gzips what the bridge embeds, so
# the files in this directory are the plain ones and a rebuild cannot leave a
# stale .gz behind next to them.

echo "wrote $out"
ls -la "$out"
echo
echo "now pack it and build the bridge, which embeds it:"
echo "  tools/pack-web.sh"
echo "  go build ./bridge"
