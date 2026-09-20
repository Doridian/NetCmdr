#!/bin/sh
# Pack the built web client into the form netcmdr-bridge embeds.
#
# The bridge serves the client from its own binary, and serves it gzipped to
# every browser that will take one — which is all of them. Compressing here
# rather than per request, or per start of the service, is the whole reason this
# script exists: the client is a WebAssembly module of several megabytes, and
# gzip takes it to under a third of that.
#
# A file gzip does not shrink — a PNG, say — is copied as it is, under no
# suffix, so that nothing lands in the binary larger than it was on disk.
#
# Building the bridge is therefore three steps:
#   tools/build-web.sh
#   tools/pack-web.sh
#   go build -C bridge -o netcmdr-bridge .
# and the resulting binary is the whole service. `nix build .#netcmdr-bridge`
# does all three.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
src="${1:-$root/crates/netcmdr-web/dist}"
out="${2:-$root/bridge/web/dist}"

if [ ! -d "$src" ]; then
  echo "no web client in $src; build it with tools/build-web.sh first" >&2
  exit 1
fi

# Regenerated whole, so a file since renamed or deleted cannot survive into this
# one. .gitkeep goes back because go:embed needs the directory non-empty.
rm -rf "$out"
mkdir -p "$out"
: > "$out/.gitkeep"

find "$src" -type f | while read -r file; do
  relative=${file#"$src"/}
  mkdir -p "$out/$(dirname "$relative")"
  # -n leaves the name and timestamp out of the header, so that an identical
  # client packs to identical bytes and the build stays reproducible.
  gzip -9 -n -c "$file" > "$out/$relative.gz"
  if [ "$(wc -c < "$out/$relative.gz")" -ge "$(wc -c < "$file")" ]; then
    rm "$out/$relative.gz"
    cp "$file" "$out/$relative"
  fi
done

echo "packed $(find "$out" -type f ! -name .gitkeep | wc -l) files into $out"
