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

# wasm-opt is optional: it roughly halves the module, but a build without it
# still works, so a missing binaryen is a note rather than a failure.
wasm="$out/b070_web_bg.wasm"
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

# Precompress what the browser has to download before anything appears. The
# bridge serves these when the client offers the encoding, so the compressing
# happens once here rather than per request.
if [ "$profile" = release ]; then
  for f in "$out/b070_web_bg.wasm" "$out/b070_web.js" "$out/index.html"; do
    [ -f "$f" ] || continue
    gzip -9 -k -f "$f"
    if command -v brotli >/dev/null 2>&1; then
      brotli -q 11 -f -o "$f.br" "$f"
    fi
  done
  wire=$(wc -c < "$out/b070_web_bg.wasm.br" 2>/dev/null \
      || wc -c < "$out/b070_web_bg.wasm.gz")
  echo "module over the wire: $((wire / 1024)) KiB"
fi

echo "wrote $out"
ls -la "$out"
