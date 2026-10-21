#!/usr/bin/env bash
# Stage one of a release: get the work onto `dev` and open a pull request.
#
# The flow is the one a repository with a `dev` branch should use:
#
#   dev  ──▶  pull request  ──▶  main  ──▶  tag v0.1.0  ──▶  the release page
#
# This script does the first half. It checks the version, runs the tests, and pushes
# the branch to `dev`. The pull request is then opened in the browser (the script prints
# the link), reviewed, and merged into `main` by whoever owns the repository.
#
#   scripts/release.sh 0.1.0
#
# The tag — and with it the release itself — is cut afterwards, from `main`, by
# `scripts/tag-release.sh`. That order matters: a release must point at a commit that
# has been reviewed and merged, not at one that only exists on a branch.
set -euo pipefail

cd "$(dirname "$0")/.."

version=${1:-}
if [ -z "$version" ]; then
  echo "usage: scripts/release.sh <version>   (for example: scripts/release.sh 0.1.0)"
  exit 2
fi

version=${version#v}
branch=$(git rev-parse --abbrev-ref HEAD)

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

slug=$(git config --get remote.origin.url | sed 's#.*github.com[:/]##; s#\.git$##')

echo "version: $version"
echo "branch:  $branch"
echo "repo:    $slug"
echo

echo "1. the tests, before anything is pushed"
cargo test --locked
cargo test --locked --manifest-path desktop/Cargo.toml
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --manifest-path desktop/Cargo.toml --all-targets --locked -- -D warnings
echo

echo "2. pushing $branch to origin"
git push origin "$branch" || {
  echo
  echo "The push failed. Nothing is published: the commits are safe locally."
  echo "Push it when you have credentials:  git push origin $branch"
  exit 1
}
echo

echo "3. open the pull request"
echo "   https://github.com/$slug/compare/main...$branch?expand=1"
echo
echo "Merge it into main when the checks are green, then cut the tag:"
echo "   scripts/tag-release.sh $version"
echo "   (or: git tag -a v$version -m \"Open Download Manager $version\" origin/main && git push origin v$version)"
