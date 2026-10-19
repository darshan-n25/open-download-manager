# syntax=docker/dockerfile:1
#
# Riptide in an image, for the machines that have no desktop: a NAS, a home server, a
# Raspberry-Pi-class box, a Kubernetes job.
#
# Two stages, because the runtime one is empty on purpose. The daemon is the whole program:
# it needs no shell, no package manager, no CA bundle and no libc, so there is nothing in the
# runtime image to install, patch or compromise but the binary itself.
#
# The build stage is Alpine, which means its default toolchain *is* musl — the result is
# already the static binary the runtime stage needs, with no cross toolchain involved.

# -- build -------------------------------------------------------------------
# `rust:1-alpine` rather than a pinned patch release: this repository's MSRV moves with
# stable, and pinning the patch here would go stale silently. Pin the digest instead if you
# need a byte-reproducible image.
FROM rust:1-alpine AS build

# `ring` (through rustls) compiles C and assembly, so the build needs a C compiler and the
# musl headers. Nothing else: no pkg-config, no OpenSSL, because TLS is rustls and its trust
# roots are compiled in.
RUN apk add --no-cache musl-dev gcc make

WORKDIR /src

# Dependencies first, so editing a source file does not re-download and rebuild the world.
COPY Cargo.toml Cargo.lock ./
RUN cargo fetch --locked

COPY src ./src
# `--locked` so the image is built from the lockfile in this commit and not from whatever
# the registry has today. `tests/` is deliberately absent: nothing here runs them.
RUN cargo build --release --locked --bin riptide

# Fail the build rather than publish an image whose entrypoint cannot start. `ldd` on a
# static binary says there is no interpreter to find.
RUN ldd target/release/riptide 2>&1 | grep -qE 'not a dynamic executable|statically linked'
RUN target/release/riptide --version

# -- runtime -----------------------------------------------------------------
FROM scratch

COPY --from=build /src/target/release/riptide /usr/local/bin/riptide

# `scratch` sets no PATH, and a bare `docker exec riptide riptide list` would then fail to find
# the binary at all (the fallback search path is `/bin:/usr/bin`).
ENV PATH=/usr/local/bin

# State, configuration and the default download directory all live under /data, which is the
# one thing to mount. `HOME` is set rather than left to chance: the layout falls back to the
# working directory when there is no home, and a container runtime that invents `HOME=/root`
# would otherwise put the queue outside the volume — a queue that cannot resume after a
# restart, which is the one thing a download manager must not do.
ENV HOME=/data
WORKDIR /data
VOLUME ["/data"]

# The daemon's port. It is only reachable from outside the container if you publish it, and
# how you publish it decides whether the token is needed (see deploy/compose.yaml).
EXPOSE 1001

# Bound to 0.0.0.0 inside the container because a container's loopback is its own: the only
# way to be reachable at all is to accept on every interface, and the publish rule on the
# host — `127.0.0.1:1001:1001` or `1001:1001` — is what really decides who can connect.
# The daemon prints a note about this at startup; in a container it is expected.
#
# The binary is PID 1 and reaps its own children, and it handles SIGTERM itself, so no init
# shim is needed for `docker stop` to be clean.
ENTRYPOINT ["/usr/local/bin/riptide"]
CMD ["daemon", "--addr", "0.0.0.0:1001"]
