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
  case "$f" in
    SIMFANG.TTF) expected="3e2d44b01c9a248a61bedae4f15c8aae501328b1f7adfe6e111a5da5aa5c5104" ;;
    simhei.ttf) expected="aa4560dd8fe5645745fed3ffa301c3ca4d6c03cbd738145b613303961ba733b8" ;;
    simkai.ttf) expected="9dd76f7ab430edd091db24c3f18e71410325c1414141aad5fe67947873ffba06" ;;
    simsun.ttf) expected="ca4da082cd970f0c8abaa79f213ddcbc475f7b5afabcb81b385998f9ebfbb53f" ;;
    xbst.ttf) expected="c52d577dd3aa719ef5fb9ff2043b7f7ca36fae468ef34a47628a3a555d006a49" ;;
  esac
  tmp="$dest/.$f.tmp"
  curl --fail --location --silent --show-error --retry 3 --max-time 120 \
    --output "$tmp" "$base/$f"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp" | awk '{print $1}')"
  else
    actual="$(shasum -a 256 "$tmp" | awk '{print $1}')"
  fi
  if [[ "$actual" != "$expected" ]]; then
    rm -f "$tmp"
    echo "checksum mismatch for $f" >&2
    exit 1
  fi
  mv "$tmp" "$dest/$f"
done
echo "done -> $dest"
