#!/usr/bin/env bash
set -euo pipefail

binary=${1:-target/release/fatresize}
for tool in truncate mkfs.fat fsck.fat stat; do
  command -v "$tool" >/dev/null || {
    echo "missing required interoperability tool: $tool" >&2
    exit 77
  }
done
[[ -x "$binary" ]] || {
  echo "build the release binary first: cargo build --release --locked" >&2
  exit 2
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

exercise() {
  local fat=$1 source=$2 target=$3
  local image="$work/fat${fat}.img"
  local recovery="$work/fat${fat}.fatrsz"

  truncate -s "$source" "$image"
  mkfs.fat -F "$fat" "$image" >/dev/null
  fsck.fat -n "$image" >/dev/null
  "$binary" --check "$image" >/dev/null
  "$binary" --dry-run --size "${target}MiB" "$image" >/dev/null
  "$binary" --force-yes --size "${target}MiB" --backup "$recovery" "$image" >/dev/null
  "$binary" --check "$image" >/dev/null
  fsck.fat -n "$image" >/dev/null

  local expected
  expected=$((target * 1024 * 1024))
  [[ $(stat -c %s "$image") -eq $expected ]]
}

exercise 16 64MiB 48
exercise 32 128MiB 96
echo "fatresize interoperability: FAT16 and FAT32 passed"
