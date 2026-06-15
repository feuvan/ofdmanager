#!/usr/bin/env bash
# Fetch the deterministic CJK fallback fonts (the Windows core fonts that
# ofd.js bundles). These give cross-platform rendering that matches the major
# OFD implementations when a document does not embed its fonts.
#
# Not committed to the repo (size + licensing). Run once after cloning.
set -euo pipefail

dest="$(cd "$(dirname "$0")/.." && pwd)/crates/ofd-core/assets/fonts"
base="https://raw.githubusercontent.com/DLTech21/ofd.js/js/src/assets"
mkdir -p "$dest"

for f in SIMFANG.TTF simhei.ttf simkai.ttf simsun.ttf xbst.ttf; do
  echo "fetching $f"
  curl -sL --max-time 120 -o "$dest/$f" "$base/$f"
done
echo "done -> $dest"
