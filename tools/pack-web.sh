#!/bin/sh
# Pack the built web client into the form netcmdr-bridge embeds.
#
# Files are gzipped once here (the wasm shrinks to under a third); files gzip
# doesn't shrink are copied unsuffixed.
#
# Step two of three (`nix build .#netcmdr-bridge` does all three):
#   tools/build-web.sh
#   tools/pack-web.sh
#   go build -C bridge -o netcmdr-bridge .
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
src="${1:-$root/crates/netcmdr-web/dist}"
out="${2:-$root/bridge/web/dist}"

if [ ! -d "$src" ]; then
  echo "no web client in $src; build it with tools/build-web.sh first" >&2
  exit 1
fi

# Regenerated from scratch. go:embed needs .gitkeep.
rm -rf "$out"
mkdir -p "$out"
: > "$out/.gitkeep"

find "$src" -type f | while read -r file; do
  relative=${file#"$src"/}
  mkdir -p "$out/$(dirname "$relative")"
  # -n for reproducible output.
  gzip -9 -n -c "$file" > "$out/$relative.gz"
  if [ "$(wc -c < "$out/$relative.gz")" -ge "$(wc -c < "$file")" ]; then
    rm "$out/$relative.gz"
    cp "$file" "$out/$relative"
  fi
done

echo "packed $(find "$out" -type f ! -name .gitkeep | wc -l) files into $out"
