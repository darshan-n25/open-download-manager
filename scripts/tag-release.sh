#!/usr/bin/env bash
# Stage two of a release: tag the merged `main` and publish.
#
# A release points at a commit that has been reviewed and merged, so the tag is created
# on `origin/main`, not on a branch. Pushing that tag is what makes the workflow in
# `.github/workflows/release.yml` build every platform and publish the release page.
#
#   scripts/tag-release.sh 0.1.0
#
# The script refuses to tag a commit that `main` does not contain: tagging a commit that
# is still on `dev` would publish a release whose code was never merged.
set -euo pipefail

cd "$(dirname "$0")/.."

version=${1:-}
if [ -z "$version" ]; then
  echo "usage: scripts/tag-release.sh <version> [--retag]   (for example: scripts/tag-release.sh 0.1.0)"
  echo
  echo "  --retag   replace a tag that already exists, pointing it at the current main."
  echo "             This is what a failed release needs: the tag is deleted and pushed"
  echo "             again, which starts the workflow a second time with the fixed"
  echo "             workflow file."
  exit 2
fi

retag=no
if [ "${2:-}" = "--retag" ]; then
  retag=yes
fi

version=${version#v}
tag="v$version"

in_cargo=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [ "$in_cargo" != "$version" ]; then
  echo "Cargo.toml says version $in_cargo, you asked for $version."
  echo "Change the version in Cargo.toml (and desktop/Cargo.toml) first."
  exit 1
fi

if git rev-parse "$tag" >/dev/null 2>&1 && [ "$retag" = no ]; then
  echo "The tag $tag already exists."
  echo "To move it to the current main (a release that failed and needs doing again):"
  echo "   scripts/tag-release.sh $version --retag"
  exit 1
fi

echo "1. fetching the merged main"
git fetch origin main --tags
echo "   origin/main -> $(git rev-parse --short origin/main)"
echo

# The check that matters: the commit that is about to be tagged has to be *in* main.
# If the pull request has not been merged, this fails and nothing is published.
if ! git merge-base --is-ancestor origin/main HEAD && \
   ! git merge-base --is-ancestor HEAD origin/main; then
  echo "The current commits are not in origin/main."
  echo "Merge the pull request into main first, then run this again."
  exit 1
fi

echo "2. tagging $tag on origin/main"
if [ "$retag" = yes ]; then
  # The old tag goes first, locally and on the remote: a tag that exists cannot be
  # moved, and the release workflow starts on the push of a *new* tag.
  git tag -d "$tag" 2>/dev/null || true
  git push origin ":refs/tags/$tag" 2>/dev/null || true
fi
git tag -a "$tag" -m "Open Download Manager $version" origin/main
echo "   $tag -> $(git rev-parse --short "$tag^{commit}")"
echo

echo "3. pushing the tag (this starts the release build)"
git push origin "$tag" || {
  echo
  echo "The push failed. The tag is created locally; push it when you have credentials:"
  echo "   git push origin $tag"
  exit 1
}

slug=$(git config --get remote.origin.url | sed 's#.*github.com[:/]##; s#\.git$##')
echo
echo "The release workflow is running. Watch it here:"
echo "   https://github.com/$slug/actions"
echo
echo "The release page, when it finishes:"
echo "   https://github.com/$slug/releases/tag/$tag"
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
