#!/usr/bin/env bash
# Package the binaries that are already built into the archives a release publishes.
#
# The GitHub workflow (.github/workflows/release.yml) does this on native runners, and
# this script does the same thing locally, from binaries you have already built. The
# names it produces are the same, so a locally built archive and a workflow-built one
# are interchangeable — which is what makes the workflow reproducible rather than
# magic.
#
#   cargo build --release --target x86_64-unknown-linux-gnu
#   cargo build --release --manifest-path desktop/Cargo.toml
#   mkdir -p dist/x86_64-unknown-linux-gnu
#   cp target/x86_64-unknown-linux-gnu/release/odm-cli dist/x86_64-unknown-linux-gnu/
#   cp desktop/target/x86_64-unknown-linux-gnu/release/odm dist/x86_64-unknown-linux-gnu/
#   scripts/package-release.sh
#
# Every directory under dist/ that holds binaries becomes one archive, and every
# archive gets a line in SHA256SUMS.txt. macOS and Windows archives have to be built
# on those systems (or by the workflow); this script only packs what it finds.
set -euo pipefail

cd "$(dirname "$0")/.."

version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
echo "packaging version $version"

# Windows archives are zips; the rest are tarballs. A zip is what a Windows user
# expects to be able to open without installing anything.
archive_for() {
  case "$1" in
    *windows*) echo zip ;;
    *) echo tar.gz ;;
  esac
}

have() { [ -f "$1" ]; }

# A zip, without assuming a zip program is installed: `zip` if it is there, Python's
# zipfile if it is not (it is always there on the machines this runs on), and bsdtar's
# `tar -a` as the last resort — which is what the Windows runner of the workflow uses.
zip_dir() {
  local dir=$1 out=$2 base
  base=$(basename "$dir")
  if command -v zip >/dev/null 2>&1; then
    ( cd "$(dirname "$dir")" && zip -qr "$out" "$base" )
  elif command -v python3 >/dev/null 2>&1; then
    python3 - "$dir" "$out" "$base" <<'PY'
import os, sys, zipfile
src, out, base = sys.argv[1], sys.argv[2], sys.argv[3]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for root, _dirs, files in os.walk(src):
        for name in files:
            path = os.path.join(root, name)
            z.write(path, os.path.join(base, os.path.relpath(path, src)))
PY
  else
    tar -C "$(dirname "$dir")" -a -cf "$out" "$base"
  fi
}

for dir in dist/*/; do
  dir=${dir%/}
  target=${dir#dist/}
  [ "$target" = "windows" ] && continue

  ext=$(archive_for "$target")
  suffix=""
  [ "$ext" = zip ] && suffix=".exe"

  staging=$(mktemp -d)
  trap 'rm -rf "$staging"' EXIT

  gui=no
  cli=no

  if have "$dir/odm$suffix"; then
    cp "$dir/odm$suffix" "$staging/"
    gui=yes
  fi
  if have "$dir/odm-cli$suffix"; then
    cp "$dir/odm-cli$suffix" "$staging/"
    cli=yes
  fi
  if have "$dir/odm-bt$suffix"; then
    cp "$dir/odm-bt$suffix" "$staging/"
    cli=yes
  fi

  if [ "$gui" = no ] && [ "$cli" = no ]; then
    echo "skipping $target: no binaries in $dir"
    continue
  fi

  cp README.md LICENSE "$staging/"

  if [ "$gui" = yes ]; then
    name="open-download-manager-$version-$target"
    pack_dir="$staging"
    mkdir -p "dist/$name"
    cp "$staging"/* "dist/$name/"
    case "$ext" in
      tar.gz) tar -C dist -czf "dist/$name.tar.gz" "$name" ;;
      zip) zip_dir "dist/$name" "dist/$name.zip" ;;
    esac
    rm -rf "dist/$name"
    echo "  dist/$name.$ext"
  fi

  if [ "$cli" = yes ]; then
    name="open-download-manager-cli-$version-$target"
    mkdir -p "dist/$name"
    cp "$staging"/odm-cli* "$staging"/odm-bt* "dist/$name/" 2>/dev/null || true
    cp README.md LICENSE "dist/$name/"
    case "$ext" in
      tar.gz) tar -C dist -czf "dist/$name.tar.gz" "$name" ;;
      zip) zip_dir "dist/$name" "dist/$name.zip" ;;
    esac
    rm -rf "dist/$name"
    echo "  dist/$name.$ext"
  fi
done

# The browser extension: a zip the user unpacks into their browser.
if [ -d extension ]; then
  name="open-download-manager-extension-$version"
  rm -rf "dist/$name"
  mkdir -p "dist/$name"
  cp extension/manifest.json extension/background.js extension/protocol.js \
     extension/popup.html extension/popup.css extension/popup.js \
     extension/README.md "dist/$name/"
  tar -C dist -czf "dist/$name.zip" "$name"
  rm -rf "dist/$name"
  echo "  dist/$name.zip"
fi

# The checksums, so a user can tell that what they downloaded is what was published.
( cd dist && sha256sum ./*.zip ./*.tar.gz 2>/dev/null | sed 's#\./##' > SHA256SUMS.txt ) || true

echo
echo "dist:"
ls -l dist
