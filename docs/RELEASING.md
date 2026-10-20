# Cutting a release

A release is a tag. Pushing `v0.1.0` makes `.github/workflows/release.yml` build every
platform, package it, publish the container image, and create the release page with the
archives attached. Nothing is uploaded by hand.

```sh
scripts/release.sh 0.1.0
```

That script checks the version, runs the tests, creates the tag and pushes it. The
workflow then does the rest.

## What the workflow builds

| Job | What it produces |
| --- | --- |
| `desktop` | the app for Windows (x64), macOS (Apple silicon and Intel) and Linux (x64), each with `odm`, `odm-cli` and `odm-bt` |
| `cli` | the headless builds, including the fully static `x86_64-unknown-linux-musl` one the container image is made from |
| `extension` | the browser extension, as a zip to unpack into the browser |
| `image` | the container image, pushed to GHCR, multi-architecture |
| `release` | the release page: the archives, `SHA256SUMS.txt`, the generated notes and the install table |

Every archive carries the binaries, the `README.md` and the `LICENSE`. The names are
the same ones the workflow produces:

```
open-download-manager-0.1.0-x86_64-pc-windows-msvc.zip
open-download-manager-0.1.0-windows-x64-setup.exe
open-download-manager-0.1.0-aarch64-apple-darwin.tar.gz
open-download-manager-0.1.0-x86_64-apple-darwin.tar.gz
open-download-manager-0.1.0-x86_64-unknown-linux-gnu.tar.gz
open-download-manager-cli-0.1.0-x86_64-unknown-linux-musl.tar.gz
open-download-manager-extension-0.1.0.zip
SHA256SUMS.txt
```

The Windows installer is built with Inno Setup
([`desktop/assets/odm.iss`](desktop/assets/odm.iss)); it is a Start-menu entry, a
desktop shortcut, an uninstaller and an optional `PATH` entry for `odm-cli`. The zip is
published next to it, because a zip is what somebody who does not want an installer
wants, and an installer is what somebody who wants a shortcut wants.

## Building the same archives locally

The packaging is a script, not a workflow-only incantation, so a release can be
reproduced on a machine that has the binaries:

```sh
cargo build --release --target x86_64-unknown-linux-gnu
cargo build --release --manifest-path desktop/Cargo.toml
mkdir -p dist/x86_64-unknown-linux-gnu
cp target/x86_64-unknown-linux-gnu/release/odm-cli dist/x86_64-unknown-linux-gnu/
cp desktop/target/x86_64-unknown-linux-gnu/release/odm dist/x86_64-unknown-linux-gnu/
scripts/package-release.sh
```

`scripts/package-release.sh` packs every directory under `dist/` into the archives
above and writes `dist/SHA256SUMS.txt`. macOS and Windows binaries have to be built on
those systems — or by the workflow, which is what a release uses.

## The version

The version lives in `Cargo.toml` and `desktop/Cargo.toml`, and the tag carries it:
`v0.1.0` is release `0.1.0`. `scripts/release.sh` refuses to tag a version the
manifest does not have, because a release whose name and contents disagree is the kind
of thing nobody notices until a user reports it.

## After the release

- The container image is tagged with the release and with `latest`:
  `ghcr.io/darshan-n25/open-download-manager`.
- The release page carries the checksums; a user verifies a download with
  `sha256sum -c SHA256SUMS.txt`.
- The README's download table points at the *latest* release, so a new tag is enough —
  no link has to be edited by hand.
