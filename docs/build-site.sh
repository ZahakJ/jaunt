#!/bin/sh
# Builds the GitHub Pages site into OUT (default .scratch/site): the landing page from
# docs/site/ at the root, the captures under captures/, and the manual (mdBook) under manual/.
# The same script runs locally and in .github/workflows/pages.yml.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here/../.scratch/site}
case $out in /*) ;; *) out=$PWD/$out ;; esac
rm -rf "$out"
mkdir -p "$out/captures" "$out/media"
cp -R "$here/site/." "$out/"
cp -R "$here/../captures/." "$out/captures/"
cp -R "$here/captures/." "$out/captures/"
cp -R "$here/media/." "$out/media/"
mdbook build "$here/manual" -d "$out/manual"
echo "site built in $out"
