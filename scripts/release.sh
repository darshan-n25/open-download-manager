#!/usr/bin/env bash
# Cut a release.
#
# A release in this project is a Git tag: pushing `v0.1.0` makes the workflow in
# `.github/workflows/release.yml` build every platform, package it, and publish a
# release page with the archives attached. This script does the part that is easy to
# get wrong — the version, the tag and the push — and prints what to watch while the
# workflow runs.
#
#   scripts/release.sh 0.1.0
#
# It refuses to run if the working tree is dirty, if the tag already exists, or if the
# version in `Cargo.toml` is not the one asked for: a release that says one version and
# contains another is the kind of thing nobody notices until a user reports it.
set -euo pipefail

cd "$(dirname "$0")/.."

version=${1:-}
if [ -z "$version" ]; then
  echo "usage: scripts/release.sh <version>   (for example: scripts/release.sh 0.1.0)"
  exit 2
fi

version=${version#v}
tag="v$version"

in_cargo=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [ "$in_cargo" != "$version" ]; then
  echo "Cargo.toml says version $in_cargo, you asked for $version."
  echo "Change the version in Cargo.toml (and desktop/Cargo.toml) first."
  exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "The working tree is not clean. Commit or stash first:"
  git status --short
  exit 1
fi

if git rev-parse "$tag" >/dev/null 2>&1; then
  echo "The tag $tag already exists."
  exit 1
fi

remote=$(git remote get-url origin 2>/dev/null || echo "")
echo "version:  $version"
echo "tag:      $tag"
echo "remote:   $remote"
echo

echo "1. the tests, before anything is tagged"
cargo test --locked
cargo test --locked --manifest-path desktop/Cargo.toml
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
echo

echo "2. tagging"
git tag -a "$tag" -m "Open Download Manager $version"
echo "   $tag -> $(git rev-parse --short HEAD)"
echo

echo "3. pushing the tag (this is what triggers the release)"
echo "   git push origin $tag"
git push origin "$tag" || {
  echo
  echo "The push failed. The tag is created locally; push it when you have credentials:"
  echo "   git push origin $tag"
  exit 1
}

echo
echo "The release workflow is now running. Watch it here:"
echo "   https://github.com/$(git config --get remote.origin.url | sed 's#.*github.com[:/]##; s#\.git$##')/actions"
echo
echo "When it finishes, the release page is at:"
echo "   https://github.com/$(git config --get remote.origin.url | sed 's#.*github.com[:/]##; s#\.git$##')/releases/tag/$tag"
echo
echo "The archives it publishes:"
echo "   open-download-manager-$version-x86_64-pc-windows-msvc.zip   the app for Windows"
echo "   open-download-manager-$version-windows-x64-setup.exe        the Windows installer"
echo "   open-download-manager-$version-aarch64-apple-darwin.tar.gz   the app for Apple silicon"
echo "   open-download-manager-$version-x86_64-apple-darwin.tar.gz    the app for Intel Macs"
echo "   open-download-manager-$version-x86_64-unknown-linux-gnu.tar.gz  the app for Linux"
echo "   open-download-manager-cli-$version-*.tar.gz                  the headless builds"
echo "   open-download-manager-extension-$version.zip                the browser extension"
echo "   SHA256SUMS.txt                                              the checksums"
