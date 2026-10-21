#!/bin/sh
# Compile the one module that knows about operating systems, for every platform we ship.
#
# `rustc --emit=metadata` resolves `std` for the *target* but never links, so it needs only
# the target's prebuilt `std` from rustup — no cross C toolchain, no linker, no SDK. That
# makes this a cheap check that `cfg(windows)` / `cfg(target_os = "macos")` code still
# compiles, which is the failure a single-platform developer hits most often.
#
# What this does NOT do:
#   * it does not *run* anything on those platforms — `.github/workflows/ci.yml` does that,
#     on a real Windows and a real macOS runner;
#   * it cannot check the whole crate this way, because the HTTP stack pulls in `ring`,
#     whose C and assembly have to be compiled by a cross compiler. CI installs one to do
#     exactly that (`cargo check --target x86_64-pc-windows-gnu`).
#
# Usage: scripts/check-portability.sh
set -eu

cd "$(dirname "$0")/.."

targets="
x86_64-unknown-linux-gnu
x86_64-unknown-linux-musl
aarch64-unknown-linux-gnu
x86_64-apple-darwin
aarch64-apple-darwin
x86_64-pc-windows-msvc
x86_64-pc-windows-gnu
"

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

installed=$(rustup target list --installed)
checked=0
skipped=0
failed=0

for target in $targets; do
    if ! printf '%s\n' "$installed" | grep -qx "$target"; then
        echo "skip    $target (rustup target add $target)"
        skipped=$((skipped + 1))
        continue
    fi
    if rustc --edition 2021 --crate-type lib --emit=metadata -D warnings \
        --target "$target" --out-dir "$out" src/platform.rs 2>"$out/err"; then
        echo "ok      $target"
        checked=$((checked + 1))
    else
        echo "FAILED  $target"
        sed 's/^/        /' "$out/err"
        failed=$((failed + 1))
    fi
done

echo
echo "$checked checked, $skipped skipped, $failed failed"
[ "$failed" -eq 0 ]
