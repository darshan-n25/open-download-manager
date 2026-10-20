# Riptide

A cross-platform download manager in the spirit of Internet Download Manager: parallel
ranged connections, crash-safe resume, a scheduler, HLS/DASH capture, FTP, browser
integration, a link grabber, and offline site archiving.

Riptide is a **clean-room reimplementation**. It was designed by statically reverse
engineering the shipped IDM binaries and then rebuilding the *behaviour* in Rust — no IDM
code is used or present. The analysis lives in
[`docs/IDM-REVERSE-ENGINEERING.md`](docs/IDM-REVERSE-ENGINEERING.md) (every claim tagged
`OBSERVED` / `INFERRED` / `UNKNOWN`), the feature-by-feature comparison, including what is
**not** implemented, lives in [`docs/FEATURE-MAP.md`](docs/FEATURE-MAP.md), and what the IDM
community actually asks for — and which of those Riptide answers — is surveyed in
[`docs/COMMUNITY-REQUESTS.md`](docs/COMMUNITY-REQUESTS.md).

> **Read the feature map before trusting a checkbox.** PAC script evaluation, FTPS, MMS and
> MPEG-TS remuxing without `ffmpeg` are honest gaps, Kerberos itself is absent (Negotiate is
> implemented with NTLM inside it), and NTLM has not been tested against a real server. §12 of
> that document checks IDM's own advertised feature list line by line and names what Riptide
> has no answer for — PAC, FTPS and MMS — having closed the four gaps it found in its own
> behaviour: periodic file synchronization, scheduled grabber projects, offline site
> archiving, and download quotas.

---

## Why it is fast

IDM's speedup is not a trick. It opens several connections to the same URL and asks each for
a different byte range (`Range: bytes=…`), writing each response directly at its final
offset. Riptide does the same, with the parts that matter recovered from the binary:

- **Ranged, resumable transfers.** `HEAD` (falling back to a 1-byte `GET`) discovers the size
  and whether the server honours ranges. `Accept-Ranges`, `ETag` and `Last-Modified` are
  recorded, and a resume sends `If-Range` so a changed file is refetched rather than corrupted.
- **Dynamic chunk re-splitting.** IDM exposes `Change chunk size to %ld` and mutates its chunk
  list (`ChLst`) while running. Riptide's segment table is live: when a worker finishes early,
  it halves the largest outstanding segment and hands the tail to an idle worker. A download
  therefore speeds up as connections free up instead of being frozen at its initial split.
- **Independent limits.** A global ceiling (bytes/second), a per-file ceiling, a per-server
  cap on simultaneous connections, and a process-wide budget over a time window ("no more
  than 500 MB per hour", IDM's *Download limits*) — exactly as IDM's settings expose them.
  The window survives a restart.
- **No end-of-download merge.** IDM assembles parts at the end. Riptide writes every chunk at
  its final offset into a preallocated sparse file and finishes with a `rename`, so the cost of
  finishing is O(1) and the file is never in a half-merged state.

## Feature overview

| Area | What you get |
| --- | --- |
| Platform | Linux, macOS and Windows from one codebase; per-platform file layout, one shared positional-write path, and archive names that are valid on Windows too — see [Platform support](#platform-support) |
| HTTP/HTTPS | Segmented ranged download, resume with `If-Range` validators, retries with backoff, redirects, cookies, custom headers, `Content-Disposition` filename handling |
| Auth | Basic, Digest (RFC 7616), Bearer, NTLMv2 (MS-NLMP written from scratch), and Negotiate/SPNEGO (RFC 4178/4559) wrapping NTLM — real Kerberos is not implemented |
| Proxies | `http`, `https`, `socks5`, IDM's compact `mode:value;bypass` strings, Tor presets |
| Queue | Persistent job store, configurable width, scheduled starts, pause/resume across restarts |
| Limits | Global and per-file rate limits, a per-server connection cap, and a bytes-per-window budget (`--quota 500M --quota-window 1h`) that pauses jobs instead of failing them |
| Integrity | Optional `--checksum sha256:<hex>` (also `sha512`, `sha1`, `md5`, or a bare digest) checked against the working file **before** it is renamed into place: a mismatch fails the job, removes the working file, and never touches the output |
| Media | HLS (`EXT-X-*`, byteranges, AES-128 keys, master playlists) and DASH (SegmentTemplate, SegmentTimeline, SegmentList); separate audio+video tracks are muxed into one file; an MPEG-TS stream `ffmpeg` cannot remux is kept as `.ts` rather than turned into a broken `.mp4` |
| FTP | Passive mode, `SIZE`, `REST`-based resume, anonymous defaults |
| Grabber | BFS crawl with depth/page limits, include/exclude wildcards, media/archive classification, tracker filtering |
| Archiving | `grab --archive` (or a `site` project with `--archive`) keeps a page and everything it needs to render — stylesheets, scripts, images, fonts — rewriting the markup to local relative paths; every URL gets a path decided by the URL alone, and anything it could not hold keeps pointing at the live site |
| Projects | Recurring work stored as one document per project: `file` keeps a URL in step with a local path using conditional requests (`304` costs no body bytes), `site` re-crawls and fetches only what changed, `site --archive` keeps an offline mirror; run by the daemon when due, or from `cron` with `project tick` |
| Browser | MV3 extension + a loopback daemon speaking IDM's `MSG#` framing over its WebSocket subprotocol, plus a plain JSON API |
| Daemon auth | Optional bearer token required on **every** route — the JSON API, the control page, `/health`, and the WebSocket handshake — compared in constant time. The token comes from `--token-file`, `RIPTIDE_DAEMON_TOKEN`, or the `daemon_token_file` setting, never from an argument; off-loopback binds say what is at stake |
| Headless / NAS | One fully static binary and a `scratch`-based image — no libc, no shell, no CA bundle, because the trust roots are compiled in: `docker compose -f deploy/compose.yaml up -d`. SIGTERM, which is what `docker stop` and `systemctl stop` send, stops running jobs at a chunk boundary, records them `paused`, and exits **0** — see [Headless and containers](#headless-and-containers) |
| CLI | `download`, `grab`, `daemon`, `probe`, `list`, `show`, `remove`, `pause`, `queue`, `project`, `config`, `state-dir` |

## Build and install

Needs a recent stable Rust toolchain (developed against 1.99). There is no `cmake`,
`pkg-config` or OpenSSL requirement: TLS is `rustls` with `webpki-roots`.

```sh
cargo build --release
install -m755 target/release/riptide ~/.local/bin/riptide
cargo test         # 337 tests: unit + protocol + end-to-end
```

On Windows the binary is the same program; only the directory it keeps its files in changes.
See [Platform support](#platform-support), including how to build a static Linux binary, and
[Headless and containers](#headless-and-containers) for the image.

## Platform support

Linux, macOS and Windows, from one codebase. Everything that differs between them lives in one
file — [`src/platform.rs`](src/platform.rs): the directory layout, the positional-write binding,
and the names Windows will not store. That is deliberate, because "does this work on X?" should
be one file to read rather than a `cfg` in every module.

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Config | `$XDG_CONFIG_HOME/riptide`, else `~/.config/riptide` | same | `%APPDATA%\riptide` |
| State: queue, quota window, projects, mirrors | `$XDG_DATA_HOME/riptide`, else `~/.local/share/riptide` | same | `%LOCALAPPDATA%\riptide` |
| Default downloads | `~/Downloads` | `~/Downloads` | `%USERPROFILE%\Downloads` |
| Home directory | `$HOME` | `$HOME` | `%USERPROFILE%`, else `%HOMEDRIVE%%HOMEPATH%` |

State goes to `%LOCALAPPDATA%` and not `%APPDATA%` on purpose: the queue points at part files
sitting on *this* disk, and config that roams to a machine without them is a queue of downloads
that cannot resume. `riptide config path` and `riptide config show` print what was resolved —
including the last resort, the working directory, if the environment names no home at all.

The layout is a pure function of the environment, so the platform table above is asserted on
whichever platform the tests happen to run on: the Windows answers are tested on Linux and vice
versa, along with `%HOMEDRIVE%`+`%HOMEPATH%`, an empty `XDG_DATA_HOME=`, and no home at all.
The tests are in [`src/platform.rs`](src/platform.rs).

```sh
./scripts/check-portability.sh
ok      x86_64-unknown-linux-gnu
ok      x86_64-unknown-linux-musl
ok      aarch64-unknown-linux-gnu
ok      x86_64-apple-darwin
ok      aarch64-apple-darwin
ok      x86_64-pc-windows-msvc
ok      x86_64-pc-windows-gnu

7 checked, 0 skipped, 0 failed
```

That script compiles the platform module for all seven targets using nothing but the prebuilt
`std` from `rustup target add` — no cross C toolchain, no SDK — so it runs on the machine you
are sitting at, and it is not vacuous: a deliberately broken `#[cfg(windows)]` function in that
file fails the `x86_64-pc-windows-msvc` line while the Linux line still passes.

`.github/workflows/ci.yml` is the rest of it: the whole suite natively on `ubuntu-latest`,
`macos-latest` and `windows-latest` with clippy and rustfmt; `cargo check --all-targets` for
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu` and `x86_64-pc-windows-gnu`; a fully
static musl binary proven static with `ldd`; `x86_64-apple-darwin` cross-built from the arm64
macOS runner; and the container image built, run and stopped. **That workflow has not run** —
this repository has no remote yet — so the non-Linux jobs are written and reviewed but
unexecuted. The executed evidence is Linux, below.

## Headless and containers

For the machines with no desktop: a NAS, a home server, a Raspberry-Pi-class box. The daemon is
the whole program, so the image is the whole program — [`deploy/Dockerfile`](deploy/Dockerfile)
builds a static binary in Alpine and copies it into `scratch`. No shell, no package manager, no
libc, and no CA bundle: TLS is `rustls` with `webpki-roots`, whose trust roots are compiled into
the binary.

```sh
docker compose -f deploy/compose.yaml up -d      # builds and runs it
curl http://127.0.0.1:1001/health
docker exec riptide riptide list
```

State, configuration, projects, mirrors and the default download directory all live under
`/data`, which is the one volume worth mounting. `HOME=/data` is set in the image on purpose:
the layout falls back to the working directory when there is no home at all, and a runtime that
invents `HOME=/root` would put the queue outside the volume — a queue that cannot resume is the
one thing a download manager must never have.

The port is published on the host's **loopback only** by default, so nothing on the network can
reach it and no token is needed. Reaching it from elsewhere means doing both halves of the change
in one edit: publish the port, and set `RIPTIDE_DAEMON_TOKEN`. With a token configured the daemon
requires it on every route, `/health` and the control page included.

**Stopping is a signal, not a kill.** `docker stop`, `systemctl stop` and `kubectl delete` all
send SIGTERM. Riptide handles it: running jobs stop at the next chunk boundary, record themselves
as `paused` with the bytes they do have, and the process exits **0** instead of dying by signal
(which is what an orchestrator reports as a crash). In a container that handler is not a nicety
but the whole mechanism: a container's PID 1 has signals with default actions *discarded* — the
kernel does not deliver them to init in a PID namespace — so without a handler `docker stop`
would send SIGTERM, have it ignored, wait out its timeout and then SIGKILL.

The same rule applies to the commands a one-shot job runs instead of the daemon. `riptide
download`, `riptide grab --download` and `riptide queue` all handle the signal, leave the job
`paused`, and exit **0**: a unit with `Restart=on-failure` must not be restarted for a stop it
asked for itself. They report the affected jobs as `stopped`, never as `failed` — a failure is a
404 or a checksum mismatch, and the message says which it is.

The stop waits up to five seconds for that saving and then reports by name anything still going,
so a wedged connection cannot hold `docker stop` open until it escalates. `stop_grace_period` /
`TimeoutStopSec` should therefore exceed five seconds — the compose file sets fifteen. A
[crawling project](#recurring-projects) is the one thing that can still be cut short: it stops at
the file boundary it has reached, and because a file is only published by an atomic rename and
the validators are written at the end of a run, an interrupted crawl re-fetches what it had not
finished rather than mistaking a truncated file for a current one.

```ini
# ~/.config/systemd/user/riptide.service
[Unit]
Description=Riptide download daemon
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=%h/.local/bin/riptide daemon --addr 127.0.0.1:1001
Restart=on-failure
# SIGTERM is handled; this is how long it may take. The daemon stops at five seconds and
# names anything that is still running.
TimeoutStopSec=15

[Install]
WantedBy=default.target
```

What is deliberately **not** in the image:

- **`ffmpeg`.** A media job still works — the fMP4 is kept, or the raw `.ts`, and nothing fails —
  but a *remuxed* MPEG-TS only happens if you bake `ffmpeg` into a derived image.
- **A healthcheck.** A `scratch` image has no shell and no `curl` to run one with, so probing
  `/health` is the orchestrator's job (`httpGet`), not the image's.
- **A user database.** Downloads run as root by default, because a bind-mounted host directory is
  usually not writable by the unprivileged uid a fresh named volume would inherit; pass
  `--user "$(id -u):$(id -g)"` and make `/data` writable by it if you would rather not.

The browser extension is not in the image either: it runs in the browser, and points at the
daemon's address. Nothing else about the daemon changes in a container — `cron`-driven
[projects](#recurring-projects) work the same way from inside the image or from the host.

**What has and has not been executed.** There is no container runtime on the machine this was
written on — no `docker`, `podman`, `buildah` or `containerd` — so `docker build` has never run
here, and neither has `docker stop`. Two things stand in: the `image` job in
`.github/workflows/ci.yml` builds the Dockerfile, publishes its port the way the compose file
does, checks that `/health` and the control page answer, stops the container with SIGTERM, and
asserts three things about the result — the exit code is 0, and the log names both the signal
and the completed shutdown. The byte-level claims are checked on the host at hand by
[`scripts/check-headless.sh`](scripts/check-headless.sh), which builds the same kind of binary —
a fully static one — and runs it inside a rootfs that holds exactly what the image holds, one
file, with an unprivileged user namespace standing in for the chroot. It is what the claims
above rest on:

```
$ ./scripts/check-headless.sh
note: checking inside a chroot with unprivileged user namespaces

== 1. a static binary ==
   11984160 bytes, ldd says: statically linked

== 2. an image that is one file ==
           40 /image/data
           60 /image/usr
           60 /image/usr/local
           60 /image/usr/local/bin
     11984160 /image/usr/local/bin/riptide

== 3. where it keeps its files ==
   no HOME    -> ./.config/riptide/config.json
   HOME=/data -> /data/.config/riptide/config.json

== 4. a real download ==
   6 MiB over 4 connections: … 100.0%  6.00 MB / 6.00 MB  —done → /data/Downloads/payload.bin

== 5. the daemon serves the API, with a job in flight ==
   GET  /health      -> 200 ok
   GET  /            -> 200
   POST /api/jobs    -> 201 {"id":"d49215ddfb764a56","status":"queued"}
   GET  /api/jobs    -> d49215ddfb764a56 is active

== 6. docker stop, mid-download ==
   exit=0 after 1s
   | SIGTERM — stopping; running jobs save their place at the next chunk boundary …
   | stopped
   the queue afterwards:
   ID                STATE    SIZE       DONE  NAME
   0dc2cdf17aa84e0f  done     6.00 MB    100%  payload.bin
   d49215ddfb764a56  paused   12.00 MB     16%  slow.bin
   d49215ddfb764a56 is paused at 2070976 of 12582912 bytes — resumable

all checks passed
```

(The rootfs is built under a temporary directory and the daemon is given a dribbling origin, so
the stop lands in the middle of a transfer rather than between two of them. The same claim is
asserted against the released binary, with no container or chroot involved, by
`cargo test --test shutdown`.)

## Use

```sh
# A single download, 16 connections, capped at 4 MB/s.
riptide download https://example.com/ubuntu.iso -n 16 --limit 4M

# Resume after Ctrl-C, a crash, or a reboot.
riptide queue

# Inspect before committing.
riptide probe https://example.com/file.iso

# Authenticated, with a referer and a SOCKS proxy.
riptide download https://example.com/private.zip \
    --referer https://example.com/page --auth alice:s3cret \
    --proxy socks5://127.0.0.1:9050

# Against IIS/SharePoint: SPNEGO (`Negotiate`), completed with NTLM.
riptide download https://intranet.corp/report.xlsx --auth 'negotiate:CORP\alice:s3cret'
# ...or the same credentials as bare NTLM messages.
riptide download https://intranet.corp/report.xlsx --auth 'ntlm:CORP\alice:s3cret'

# Capture a stream, preferring 1080p, from a manifest URL.
riptide download https://cdn.example.com/master.m3u8 -q 1080

# Find what a page offers, then fetch it.
riptide grab https://example.com/downloads --depth 2 --include '*.zip' --include '*.iso'
riptide grab https://example.com/downloads --download --output-dir ~/Downloads

# Keep a page and everything it needs to render, rewritten to work from disk.
riptide grab --archive -o ~/Offline https://example.com/docs/

# See the queue.
riptide list
riptide show 3f1a9c02

# Stay inside a data budget: at most 500 MB per hour, shared by every job.
riptide queue --quota 500M --quota-window 1h
# ...or make it the standing policy.
riptide config set quota_bytes 500M
riptide config set quota_window_secs 3600

# Only publish the file if its bytes are the ones you expect.
riptide download https://example.com/ubuntu.iso --checksum sha256:9f86d081884c7d65...
# A bare digest works too, and its length says which algorithm it is.
riptide download https://example.com/ubuntu.iso --checksum 9f86d081884c7d65...

# Keep a file in step with one URL, hourly, and fetch the first copy now.
riptide project add feed --url https://example.com/feed.json --every 1h --start-now
# Re-crawl a page daily and fetch whatever it offers.
riptide project add mirror --kind site --url https://example.com/downloads \
    --every 1d --download -o ~/Mirror
# Or keep a daily offline mirror of a section of a site.
riptide project add offline --kind site --url https://example.com/docs/ \
    --every 1d --archive -o ~/Offline
# The daemon runs due projects; `cron` users can run them from a timer instead.
riptide project tick
```

A job that runs out of budget is **paused**, not failed: the partial file is kept and
`riptide queue` continues it once the window refills.

`riptide` keeps its configuration and job state under your platform's config/data
directories; `riptide config path` and `riptide state-dir` print the exact locations.
Every setting is editable from the CLI, and byte-valued settings accept the same `500M`
shorthand:

```sh
riptide config show
riptide config set connections 16
riptide config set speed_limit 4M
riptide config set quota_bytes 500M
```

## Recurring projects

IDM's *periodic synchronization* and its *scheduled site grabber* are the same idea — a
named definition meant to run again — so Riptide stores both the same way, one JSON
document per project under `state_dir/projects/`, and selects the behaviour with `--kind`.

```sh
riptide project add feed --url https://example.com/feed.json --every 1h --start-now
riptide project add mirror --kind site --url https://example.com/downloads \
    --every 1d --download -o ~/Mirror
riptide project add offline --kind site --url https://example.com/docs \
    --every 1d --archive -o ~/OfflineDocs

riptide project list           # name, interval, when each next runs, how the last run went
riptide project show feed
riptide project run feed       # now, whatever the schedule says
riptide project tick           # everything that is due — the command for cron
riptide project disable feed   # stop it running, keep the files
riptide project remove feed    # forget the schedule, keep the files
```

A `file` project asks the server whether the copy on disk is still current before moving
any bytes: it sends `If-None-Match` / `If-Modified-Since`, and a `304 Not Modified` ends
the run with nothing transferred. Anything else is a new version and is fetched **in full**
into the engine's working file, then swapped into place, so a reader never sees a
half-written file and a run that fails cannot damage the copy that was already there.

Three consequences are deliberate and worth knowing:

- **A sync never resumes a partial download across runs.** Bytes left in the working file
  belong to a version the server has since replaced, and splicing old and new bytes
  together produces a file that is quietly corrupt. When the remote turns out to have
  changed, the working file is discarded first. Resume still works *within* a run, so a
  dropped connection costs only the bytes in flight.
- **The next run is measured from the end of the previous one**, so a run slower than its
  own interval cannot queue up a backlog; a daemon that was down for a week runs each
  project once when it comes back, not once per missed slot.
- **Running out of quota is not a failure.** The run stops, the project is marked
  `quota-paused`, and the next interval tries again — the same rule the interactive queue
  follows.

A `site` project re-runs the grabber and, with `--download`, fetches what it finds. Each
target gets the same conditional treatment as a `file` project, so a second run of an
unchanged site costs one request per target and no body bytes. Targets are fetched one at a
time on purpose: an unattended job should be gentle with the origin. `--max-downloads`
(500 by default) caps how many a single run will pull from a page that lists thousands.

`riptide daemon` runs due projects itself, up to 30 seconds late, and the control page
lists them. Everything a project transfers goes through the same engine as an interactive
download, so it is governed by the same global and per-file speed ceilings, the same
per-server connection ceiling and the same quota.

## Offline site archiving

`--archive` turns the same crawl into a mirror you can read with the network off: the pages,
plus the stylesheets, scripts, images and fonts they need in order to look right, with the
markup rewritten so the copy works from disk.

```sh
riptide grab --archive -o ~/Offline/docs https://example.com/docs/
riptide project add offline --kind site --url https://example.com/docs/ \
    --every 1d --archive -o ~/Offline/docs
```

Four rules decide what ends up on disk, and they are worth knowing before trusting the
result:

- **A URL's place is decided by the URL alone.** `https://site/dir/page.html` becomes
  `dir/page.html`; a URL on another host goes under `_hosts/<host>/` so that two sites with
  the same paths cannot collide; `?a=1` and `?a=2` are different files. Nothing depends on
  visit order, so re-running an archive overwrites its own files rather than accumulating
  copies. The one deliberate alias is that `/dir/` and `/dir/index.html` are the same
  document, because they are.
- **A reference the run could not hold keeps pointing at the live site.** Whatever a filter
  excluded, a page past the depth limit, or a target that 404'd stays an absolute URL — a
  link the reader can see is remote, rather than a local path that is not there. The run
  prints how many it left that way, and they are counted in the JSON report.
- **Markup and stylesheets are re-read every run; everything else is asked about first.**
  HTML and CSS are the maps to everything else, so a `304` on them would mean never noticing
  that an image they point at had changed. They are small; images, fonts and scripts carry
  `If-None-Match`, so a run in which nothing changed moves no bodies at all and reports
  `unchanged`. What the server said is kept in `_riptide/validators.json` **inside the
  archive**, so a mirror that is copied or moved keeps its record and the next run is still
  incremental — a repeat `grab --archive` costs one conditional request per asset, not a
  re-download.
- **An archive is not a mirror of a server.** Video, installers and archives are things to
  *download*, not things a page needs in order to render, so they are not swept up — point
  `riptide grab --download` at the page for those. `--max-assets` (or `--max-downloads` for a
  `site` project) caps how many assets a single run will hold.

## Browser integration

```sh
riptide daemon                       # 127.0.0.1:1001
```

Then load `extension/` as an unpacked extension. The extension cancels the browser's own
download and re-queues it with the daemon, forwarding cookies, `Referer` and `User-Agent` so
authenticated downloads keep working. It also scans the current page for media and offers a
"Download with Riptide" context menu. See [`extension/README.md`](extension/README.md) for the
protocol tables and the permission rationale.

Scripts can use the daemon directly:

```sh
curl -s http://127.0.0.1:1001/api/jobs
curl -s http://127.0.0.1:1001/api/quota      # bytes used / left in the current window
curl -s http://127.0.0.1:1001/api/projects   # recurring projects and when each next runs
curl -s -X POST http://127.0.0.1:1001/api/jobs \
     -H 'content-type: application/json' \
     -d '{"url":"https://example.com/big.iso","connections":16}'
curl -s -X POST http://127.0.0.1:1001/api/projects/feed/run \
     -H 'content-type: application/json'          # run a project now
```

A POST has to declare `application/json`, including one with no body. See
[Authentication](#authentication) for why that rule is load-bearing rather than pedantic.

### Authentication

By default the daemon has none, which is why it listens on `127.0.0.1` and nowhere else. To
drive it from another machine — a NAS, a container, a second laptop — give it a token and bind
it wider:

```sh
riptide daemon --generate-token-file ~/.config/riptide/token   # 32 random bytes, mode 0600
riptide daemon --token-file ~/.config/riptide/token --addr 0.0.0.0:1001
```

Once a token is set **every** route needs it: the JSON API, the control page, `/health`, and the
WebSocket handshake the extension uses.

```sh
curl -s -H "Authorization: Bearer $(cat ~/.config/riptide/token)" http://127.0.0.1:1001/api/jobs
curl -s "http://127.0.0.1:1001/api/jobs?token=$(cat ~/.config/riptide/token)"   # same thing
```

Where the token can come from, in order of precedence:

| Source | Why it exists |
| --- | --- |
| `--token-file PATH` | Per-run control; a *path*, so the secret is not in `argv`, which `ps` shows to every user |
| `RIPTIDE_DAEMON_TOKEN` | Containers and service managers, which can inject a secret without a file |
| `daemon_token_file` setting (`riptide config set daemon_token_file /etc/riptide/token`) | Set once and forgotten; `config set … none` unsets it |

A configured source that cannot be read (missing file, empty file, empty variable) is a **startup
failure**, never a quiet fall back to "no token". A misspelled path must not turn authentication
off. The extension has a **Daemon token** field under Settings, which it sends in the WebSocket
URL — a browser cannot set a header on a handshake.

What a token does and does not buy you: the daemon speaks plain HTTP, so a token stops someone
who can reach the port but not watch the wire. Publicly reachable means putting TLS in front of it
(a reverse proxy, or an SSH tunnel), and the daemon says so on startup when it is bound off
loopback.

## Architecture

```
src/
  main.rs        entry point, logging, exit codes
  cli.rs         argument parsing, job construction, progress rendering
  config.rs      settings, download-folder categorisation, output naming
  job.rs         Job / Segment / RequestContext / MediaSpec — the persisted model
  segment.rs     segment planning and runtime splitting (IDM's ChLst analogue)
  engine.rs      the transfer engine: probe, ranged workers, retry, re-split, single-stream fallback
  storage.rs     preallocated sparse sink with positional writes + atomic finalize
  checksum.rs    md5/sha1/sha256/sha512 digests, verified before the final rename
  state.rs       crash-safe job store (temp file + fsync + rename per job)
  throttle.rs    global / per-file token-bucket rate limiting
  quota.rs       bytes-per-window budget ("Download limits"), persisted across restarts
  scheduler.rs   queue width, scheduled starts, cancellation propagation
  auth.rs        Basic, Digest, Bearer; spec parsing and challenge handling
  auth/ntlm.rs   NTLMv2 (NTOWFv2, Type 1/2/3 messages) from MS-NLMP
  auth/spnego.rs SPNEGO (RFC 4178/4559): hand-rolled DER, wrapping, mechanism classification
  auth/token.rs  the daemon's own bearer token: constant-time compare, file/env sources
  net/           client construction, TLS policy, proxy parsing and resolution
  media/         HLS resolver, MPD parser, muxing and remuxing
  grabber.rs     link extraction, filtering, crawl
  archive.rs     offline mirroring: URL→path placement, markup rewriting, asset fetching
  project.rs     recurring projects: periodic file sync, scheduled site crawls, due-time logic
  ftp.rs         passive-mode FTP with REST resume
  daemon.rs      `MSG#` WebSocket service + JSON HTTP API + bearer-token gate + project ticker
extension/       MV3 browser integration (background, popup, options)
tests/           end-to-end tests against servers that really do byte ranges, conditional requests and SPNEGO, plus daemon protocol tests and an archive suite that reads its own output back
```

## What the community asks for

[`docs/COMMUNITY-REQUESTS.md`](docs/COMMUNITY-REQUESTS.md) surveys what IDM users and IDM
*refugees* ask for, and Riptide's answer to each. The headlines:

| Ask | Where it comes from | Riptide |
| --- | --- | --- |
| Run on macOS/Linux at all | IDM's own FAQ confirms Windows-only; the loudest theme in Linux/macOS threads | Native binaries, no Wine — but only Linux is built and tested today; see the [roadmap](docs/ROADMAP.md) |
| Browser integration that survives browser policy | Chrome disabling IDM's MV2 extension; IDM's own "keep the extension enabled" advice | Loopback daemon is the integration; the extension is a thin MV3 client, and `curl` can do everything it can |
| Never lose media again, and say so honestly when it cannot | The video panel is the reason people stay; protected streams and YouTube changes are the top failures | HLS/DASH capture and muxing; no DRM circumvention and no per-site signature layer, by design |
| Speed, resume, batch | "up to 8 times faster", "paste 100 links", resume after a crash | Dynamic segmentation, conditional resume, queue; batch is the shell and the daemon API |
| BitTorrent and magnet links | The single most-cited reason to keep a second app installed | `riptide-bt` and the window's Torrents tab: `.torrent` files and magnet links, pieces verified by SHA-1. A second engine, not a mode of the HTTP one |
| Checksum verification | Listed as a headline feature by neighbouring projects | `--checksum sha256:…`, verified before the rename |
| Headless / NAS / remote control | "headless NAS server", aria2's "RPC daemon mode" | Daemon + JSON API + `project tick` + an optional bearer token on every route, and a `scratch` image that runs it all: `docker compose up -d`, `docker stop` and back without losing a queue |
| No ads, no nag, no account | The most common reason given for choosing an alternative | Nothing to build: no account, no telemetry, no nag |
| Offline copies of sites | IDM advertises it; the strings are in the binary | `grab --archive`, `site --archive` |

## Roadmap

[`docs/ROADMAP.md`](docs/ROADMAP.md) re-checks the four limitations this project has carried
and ranks what to build next. The ranking, in short:

**Done since that list was written:**

1. **Checksum verification** (`--checksum sha256:…`, also `sha512`/`sha1`/`md5`) — checks the
   working file before the rename, so a mismatch never becomes the output and never replaces a
   good file. Works for `download`, `grab --download`, a `file` project, and the daemon's
   `POST /api/jobs`.
2. **MPEG-TS remux hardening** (was item 6) — a failing `ffmpeg` now falls back to keeping the
   raw `.ts`, where before only a *missing* `ffmpeg` did.
3. **Daemon authentication** (was item 2) — a bearer token from `--token-file`,
   `RIPTIDE_DAEMON_TOKEN` or a config path, required on every HTTP route and on the WebSocket
   handshake, compared in constant time. Off-loopback binds now carry a startup note saying what
   the token does and does not protect.
4. **Linux + macOS + Windows support** (was item 1) — one `src/platform.rs` holding the layout,
   the positional-write binding and the reserved names; the download-name and archive-name
   sanitising now produces names Windows will actually store; a runtime-verified platform table
   with tests for both platforms; a seven-target compile check that runs on one machine; and a
   CI matrix that runs the suite natively on all three systems, cross-checks three more targets,
   and proves the static musl build is static. See [Platform support](#platform-support) for
   what has and has not been *executed*.
5. **Headless packaging** (was item 4) — the daemon learned to stop on SIGTERM, which it had
   never done: `docker stop` and `systemctl stop` used to kill it mid-write. It now stops at a
   chunk boundary, waits up to five seconds for running jobs to save themselves as `paused`, and
   exits 0. Around that: a `scratch` image with no libc, shell or CA bundle
   ([`deploy/Dockerfile`](deploy/Dockerfile)), a `.dockerignore` so the three-gigabyte
   `target/` never enters the build context, a compose file that publishes on host loopback
   unless you also set a token, a release workflow that attaches static musl / glibc / macOS /
   Windows archives and pushes the image to GHCR, and a CI job that builds the image, serves
   `/health` and `/` from it, stops it with SIGTERM and checks both the exit code and the log.
   Auditing the item afterwards turned up two things it had not covered, both fixed here: a
   stop asked for by a signal was still reported as a **failure** by `riptide download`,
   `riptide grab --download` and `riptide queue` — the first two exited 1 as well, so a unit
   with `Restart=on-failure` would restart on a stop it had asked for itself — and the
   Dockerfile's static check used Alpine's `ldd`, whose output for a static binary is not
   glibc's. See [Headless and containers](#headless-and-containers).

**The window.** `odm.exe` (or `odm` on Linux) is a native application — not a web page in
a browser — with the layout the brief asked for: a menu bar, a toolbar of icon actions
(Add URL, Resume, Stop, Delete, Delete Completed, Options, Scheduler, Start Queue, Stop
Queue), a category tree down the left (All Downloads, Unfinished, Finished, Failed, and
per-type categories), and a sortable table with File Name, Size, Status, Time left,
Transfer rate, Last Try and Description. It is styled dark and flat, the way a modern
application should be, rather than the grey of a 2000s-era program.

It is standalone: it starts the daemon in-process, so running the program is all there is
to do. The Torrents tab takes a `.torrent` file — dropped onto the window or typed — and a
magnet link, and shows the transfer as it happens.

**Still to build, in the order the roadmap ranks them:**

1. **BitTorrent as a sibling binary** — the most-requested feature in the ecosystem, sharing
   the queue and state but deliberately **not** a mode of the HTTP engine. (Shipped as
   `riptide-bt`, and in the window's Torrents tab, with magnet links.)

Nothing else on the roadmap's numbered list remains. The two things that were recorded as
"cannot be verified on this host" are now handled the only way they can be without the
missing tooling:

- **MPEG-TS remux, success path.** A valid 27 KiB transport-stream fixture now ships at
  `tests/fixtures/remux/sample.ts` (generated by ffmpeg itself, which can *write* TS even
  though it segfaults *reading* one), and `media::remux::tests::a_working_ffmpeg_remuxes_a_
  transport_stream_to_mp4` is an `#[ignore]`d test that remuxes it and asserts the result is
  an MP4 that is not a verbatim copy of the input. It is ignored because this host's ffmpeg
  cannot pass it; a host or CI runner with a working ffmpeg runs it with
  `cargo test -- --ignored`. The fallback is already proven here by
  `a_present_but_failing_ffmpeg_still_keeps_the_raw_ts` and
  `a_real_ffmpeg_can_never_lose_an_mpeg_ts_track`. See [docs/ROADMAP.md](docs/ROADMAP.md).
- **Container image.** Still unbuildable here (no container runtime); the `image` job in CI
  and `scripts/check-headless.sh` stand in for it.

The same document records what verification changed on 2026-10-19: ffmpeg turned out to be
installed (but crashes on MPEG-TS input, so the remux path still cannot be verified), and the
IDM binary turned out to still be on disk and disassemblable with `objdump` — 854,785
instructions — where the original report said no disassembler was ever available.

## How Riptide differs from IDM

Some of IDM's mechanisms are load-bearing for its business model and not for downloading.
Riptide keeps the parts that make transfers fast and drops the parts that exist to own the
browser.

**Kept, because they are the actual engineering:**

- A hand-rolled transfer path with `Range`/`If-Range`/`Content-Range` handling rather than a
  library that hides the primitives.
- A mutable chunk table with runtime re-splitting.
- Independent global, per-file, per-server and per-window limits.
- A thin browser extension paired with a privileged local daemon: the browser supplies session
  state, the daemon does the work.

**Dropped, deliberately:**

| IDM | Riptide | Reason |
| --- | --- | --- |
| Kernel filter drivers (`idmwfp*.sys`, `idmtdi*.sys`) | None | Needs kernel code, driver signing and admin rights to do what a userspace proxy client already does |
| Redirects browser traffic through IDM's proxy | Nothing — Riptide runs its own HTTP stack | No interception of unrelated browsing, and no TLS man-in-the-middle requirement |
| `declarativeNetRequest` rules to silently rewrite downloads | `downloads.onCreated` + explicit cancel | Visible, reversible, and hands the download back to the browser if the daemon is down |
| Native messaging host (`IDMMsgHost.exe`) trusted by PID | WebSocket on loopback, optional bearer token off it | No registry keys or per-OS host manifests, and no "trust whatever connects" assumption — the peer presents a secret instead of being inferred from a process id |
| Merge-based completion | Direct positional writes + `rename` | The merge step is the single most common IDM complaint |

## Known limitations

Tracked in full in [`docs/FEATURE-MAP.md`](docs/FEATURE-MAP.md) — including a line-by-line
check of [Tonec's own feature list](https://www.internetdownloadmanager.com/features2.html)
against this implementation (§12 there). The short version:

**Protocol and security**

- **Kerberos/Negotiate**: `negotiate:` works by wrapping NTLMv2 in SPNEGO, which is what a
  server that accepts NTLM inside `Negotiate` expects — IIS's default provider order. **Actual
  Kerberos is not implemented**: there is no KDC client, so a Kerberos-only server is detected
  and reported in those words instead of failing obscurely. The `mechTypes` list offers only
  NTLMSSP, because RFC 4178 §3.1 forbids offering a mechanism you cannot complete. The whole
  path is covered end-to-end against a mock server implementing RFC 4559 and MS-NLMP, because
  no real Windows or domain-joined host was available.
- **NTLMv2 is unverified against a live server** — the message layout and hash are tested
  against published vectors, and the full handshake runs end-to-end (including NTProofStr
  verification) against a mock server written from MS-NLMP, but no interop run against real
  IIS/Exchange/SharePoint or a domain-joined host has happened.
- **PAC scripts are not evaluated**: no in-process JavaScript engine. A PAC URL logs a warning
  and falls back to the ambient proxy configuration.
- **FTP is single-connection**, `ftps://` is explicitly refused rather than silently treated as
  plaintext, and an **FTP proxy** is not supported.
- **MMS is not supported.** IDM really implements it (`MMSCn` in its own strings), but the
  protocol was dropped by Windows Media Services in 2008 and `mms://` links are effectively
  extinct.
- **`multipart/byteranges` responses are not parsed** (Riptide always asks for one range per
  request, so a conforming server never sends them).

**Daemon**

- **A token is not a substitute for TLS.** The daemon speaks plain HTTP and a plain `ws://`
  handshake, so a bearer token stops someone who can reach the port but cannot watch the wire.
  Anything reachable beyond a trusted network needs TLS in front of it (a reverse proxy, or an
  SSH tunnel); the daemon prints that note itself when bound off loopback.
- **No token means no authentication at all**, which is why the default bind is `127.0.0.1`.
  What limits the damage of a tokenless loopback daemon is that its API is not reachable from a
  web page: `POST` requires `Content-Type: application/json`, which browsers preflight, and no
  CORS headers are ever sent. A local *process* running as your user can still drive it — as it
  could edit the state directory directly.
- **One token, no identities.** There is no per-client token, no expiry, no rotation API and no
  failure rate-limiting: the question it answers is "may this client talk to my daemon at all",
  which is what binding off loopback needs, and deliberately not more.
- **The token rides in the WebSocket query string** for browser clients, because a handshake
  cannot carry a header. A URL can end up in a log in a way a header cannot, so the header form
  is preferred everywhere it is possible. Nothing about this affects the default loopback setup,
  which needs no token.
- **Failed authentication is logged, not delayed.** A wrong token gets a 401 immediately rather
  than a backoff. With a 256-bit secret that is the right trade, but it does mean a token that is
  a short password is only as strong as that password.

**Headless and containers**

- **The image has never been built here**, and neither has `docker stop`: this machine has no
  container runtime at all. The `image` job in CI and `scripts/check-headless.sh` are what stand
  in for it — the latter runs a *glibc* static build rather than the musl one the Dockerfile
  produces, which is the same claim (no interpreter, no shared libraries) from a different libc.
  For the same reason the Dockerfile's own static check is `readelf` — the interpreter segment is
  present or it is not, whatever the libc — rather than Alpine's `ldd`, whose wording for a static
  binary differs from glibc's and cannot be compared against anything on this host.
- **The crawl in a stop is cut at the grace deadline, not at a file boundary.** For a [site
  project](#recurring-projects) the daemon exits after five seconds wherever the crawl has got to;
  the files already renamed are kept and the run's validators are not written, so the next run
  re-fetches what is missing instead of believing a truncated file.
- **Nothing is remuxed inside the image.** `ffmpeg` is not installed, so an MPEG-TS capture keeps
  the raw `.ts` there. Bake `ffmpeg` into a derived image if you need the remux.
- **No healthcheck, and no shell to write one in.** Probing `/health` is the orchestrator's job.
- **The shutdown grace is five seconds.** A job wedged mid-chunk cannot be cancelled before that;
  the daemon then names what is still running and exits anyway, leaving a resumable part file.
  A `stop_grace_period` (or `TimeoutStopSec`) below five seconds means the service manager's
  SIGKILL arrives first, which is the case this feature exists to avoid.
- **Downloads run as root** unless you pass `--user`, because the image carries no user database
  and a bind-mounted host directory is usually not writable by an arbitrary uid.

**Media**

- **MPEG-TS remux needs `ffmpeg` on `PATH`.** When `ffmpeg` is missing **or fails**, the raw
  `.ts` is kept and a warning is printed, rather than emitting a broken `.mp4` or losing the
  download. Set `RIPTIDE_FFMPEG` to a specific binary to override the lookup. The remux to a
  *valid* `.mp4` has never been executed here — this host's `ffmpeg` segfaults on every
  MPEG-TS input — so what is tested is that no input can make the capture *fail*.
- **Live HLS playlists are not re-polled** — a capture covers the manifest as it stood when it
  was read.
- **No live-stream sniffing.** A player that fetches its manifest from JavaScript will not be
  detected; paste the manifest URL, or let the extension's page scan find it.

**Integrity**

- **One checksum describes one file.** `--checksum` applies to a single download: it is refused
  for multiple URLs, for a `site` project, and for `--archive`, rather than silently checking
  the first file and ignoring the rest.
- **Media output is checked after it is assembled**, not before a rename — HLS/DASH write their
  output directly instead of renaming a working file — so a mismatch removes the file just
  built instead of preventing it from appearing.
- **A checksum is an extra gate, not a substitute for HTTPS.** It proves the bytes match an
  expectation you already have from somewhere else; it does not tell you where to get that
  expectation. Riptide never fetches a `.sha256` sidecar or trusts a digest served over the
  same connection.

**Limits**

- **A quota pauses between chunks, not inside one.** If the remaining budget cannot fit the
  next chunk the charge is refused outright, so a job can pause with up to one chunk of budget
  unspent. It continues on the next `riptide queue` after the window refills. The window is a
  fixed span from the first byte charged, not a wall-clock bucket.

**Recurring projects**

- **A server with neither `ETag` nor `Last-Modified` cannot be synchronized, only
  re-downloaded.** Such a run logs a warning and fetches the file again every time, because
  there is nothing to ask the server about. Riptide does not guess with a size or a
  timestamp.
- **The daemon's project ticker polls every 30 seconds**, so a project runs up to half a
  minute after its scheduled time. Missed slots are not replayed: a project that was
  overdue by anything runs once and is rescheduled from the end of that run.
- **A `site` project fetches its targets one at a time.** Per-server ceilings still apply,
  but a large mirror is slower than the interactive grabber, which is the intended
  trade-off for an unattended job.
- **A `site` project never deletes anything.** A target that disappears from the page, or
  starts 404ing, is reported as a failed target; the local copy is left alone.
- **Project artifacts are addressed by URL, not by content.** Two different URLs that
  resolve to the same file name land in the same output path for a `file` project only if
  the user chose that path; for a `site` project, a URL with no file name of its own gets
  one derived from its host and path so two index pages cannot collide.

**Archiving**

- **The rewrite is regex-based over attributes and `url(...)`, not a real HTML parser.**
  That is deliberate — HTML5 in the wild is not XML, and a tag-soup parser written for this
  would have its own bugs — but it means a URL assembled by JavaScript at run time is not
  seen. A page whose images are only referenced from script will archive as a page with
  remote images.
- **Only UTF-8 and the single-byte charsets are decoded.** A page in, say, Shift-JIS is
  decoded as UTF-8 with replacements and re-written that way, which is reported rather than
  done silently. Everything else keeps the charset it was served with, because a binary
  asset is never re-encoded.
- **An archive never deletes a file.** A reference that disappears from a page leaves its
  old copy on disk, so a mirror is a union of what has been seen, not a snapshot.
- **Assets are swept up by extension, not by what the page actually uses.** A page that
  links a `.pdf` keeps it remote; a page that links a `.json` gets it. `--exclude` and
  `--include` are the same knobs the plain grabber uses, so a filter learned once applies
  to both.
- **A server that offers neither `ETag` nor `Last-Modified` makes every asset look
  changed**, so such an archive re-transfers its assets each run even when nothing moved.
- **The manifest is rewritten from what a run saw**, so a URL that no page references any
  more loses its entry — while its file stays on disk, as above.

**Whole features IDM has that Riptide does not**

- **No PAC evaluation, no FTPS, no MMS.** See the protocol section above for each.
- **No antivirus-on-completion hook**, no self-updater, no installation wizard, and no UI
  languages other than English.
- **No BitTorrent features beyond the basics**: single-file torrents only, HTTP trackers
  only, no DHT, no peer exchange, no seeding, no multi-file torrents. IDM has no BitTorrent
  at all, so this is already more than it has — but a modern client would do more.

**Platform and interface**

- **No Windows shell integration** (IE/context-menu hooks). The CLI and the extension are the
  interfaces.
- **No GUI, so no skins, drag-and-drop, or shutdown/hang-up-when-done scheduler actions.**
- **Windows preallocates the whole file.** The sink sets the final length up front so a download
  that cannot fit fails immediately rather than at 99%; on Linux and macOS that length is sparse
  and costs nothing, while on Windows the clusters are reserved, so a 4 GB download needs 4 GB
  free before it starts. Setting the sparse attribute is not done deliberately: it needs a
  `DeviceIoControl` call that cannot be exercised from this project's development machine, and
  untested unsafe filesystem code is a worse trade than reserving the space.
- **With no home directory in the environment, both platforms fall back to `.`** — the working
  directory — exactly as before this was made platform-aware. Nothing is written until a command
  needs it, and `riptide config path` prints what was resolved rather than leaving you to guess.
- **A deep archive can exceed Windows' 260-character `MAX_PATH`**, because one URL becomes one
  nested path and `std` does not add the `\\?\` prefix for you. Shorten the archive root, or
  enable long paths on the machine.
- **The daemon's token file carries no mode on Windows.** `--generate-token-file` writes `0600`
  on Unix; on Windows the file inherits the ACL of its directory (`%LOCALAPPDATA%\riptide` is
  per-user by default), because inheriting the correct ACL beats guessing one.
- **Completion is a `rename`,** which on Windows fails with a sharing violation if another
  process holds the part file open *without* `FILE_SHARE_DELETE` (a virus scanner, usually).
  Riptide's own handle is not a problem — Rust opens files with share-delete — and the failure is
  reported with the file left exactly where it was, so a retry succeeds once the process lets go.
- HTTP/2 is requested but **multiplexing is not exploited**: each range is still its own
  request stream, and the connection count is what drives the speedup.

## Testing

```sh
cargo test                              # Rust: 337 tests
node --test extension/tests/protocol.test.mjs   # JavaScript: 15 tests
./scripts/check-portability.sh          # the platform module, for all 7 targets
./scripts/check-headless.sh             # the static binary alone in a one-file rootfs
```

337 Rust tests: 268 unit tests across every module, 17 daemon-protocol tests driving a real
WebSocket client against a bound daemon, 9 end-to-end tests that stand up an HTTP server
which really does byte ranges, 6 that stand up a mock `Negotiate` server, 6 that verify
checksums end-to-end against a real HTTP server, 17 that stand up a server which really
honours `If-None-Match` and `Range`, 10 that archive a small site and then read their own
output back to check that every rewritten reference resolves, and 4 that start the released
**binary**, send it a real signal, and check what it does about it — three to the daemon
(SIGTERM mid-download, SIGINT, an idle stop that must not wait) and one to a foreground
`riptide download`.

The end-to-end ones assert that

- the ranged requests **tile the file exactly** — no gap, no overlap, more than one request;
- a server that truncates responses produces a *failed* job and **no** file published as final;
- the next run **reuses the partial data** and produces a byte-identical file, serving strictly
  fewer bytes than the whole file;
- a server that refuses ranges still yields correct bytes in single-stream mode;
- the **per-server ceiling** is honoured: a job that asks for 8 connections while the ceiling is
  2 never makes the server see more than 2 at once, and still uses both slots;
- a **download quota** stops a job mid-transfer as `paused` (not `failed`), keeps the partial
  file, and a later run finishes it byte-identically; and that a **restarted** process does not
  hand back a fresh budget;
- a **checksum** that matches publishes the file byte-identically on both the segmented and the
  single-stream path, and a checksum that does not match fails the job, removes the working
  file, publishes nothing, and leaves an **existing good file untouched**;
- **every route is 401 without the daemon's token** and 200 with it, in either the header or the
  query form, including the control page and `/health`; the WebSocket handshake is refused
  outright without it, and the token is compared in constant time;
- a **`POST` a web page could have sent** — `text/plain`, a form encoding, or no content type at
  all — is refused, and queues nothing;
- the grabber follows exactly one level per `depth` and filters trackers;
- a **`Negotiate` job completes a real SPNEGO handshake** against a mock server that unwraps the
  token, issues an NTLMSSP challenge inside a `NegTokenResp`, and verifies the client's NTProofStr
  against the challenge it issued, then downloads the file byte-identically — and that a
  Kerberos-only server, a wrong password, and a server offering only bare NTLM each fail with a
  message naming the actual problem.

And the project tests assert the properties that only show up *between* runs:

- an unchanged remote is answered `304` and **not one body byte moves**, while the file on disk
  stays exactly as it was;
- a changed remote is refetched **in full** — including when a stale working file from the
  previous version is sitting there, which must never be spliced into the new version;
- a run that fails leaves **the previous version intact** and records why on the project;
- a `site` project fetches only what the crawl found, skips targets that are already current on
  the next run (one conditional request each, no bodies), re-fetches just the one that changed,
  and survives a target that 404s without losing the others;
- the daemon's due-sweep path runs exactly the projects whose time has come, persists each
  result, and reschedules from the end of the run rather than replaying missed slots.

And the archive tests assert the properties that make the mirror readable with the network
off:

- every reference in every rewritten document — and in the stylesheet's `url(...)`s —
  resolves to a file that is actually in the archive;
- a reference the run could not hold (an excluded one, one past the depth limit) keeps the
  **live URL** rather than a local path that is not there;
- a second run of an unchanged site transfers **only the pages**, gets a `304` for every
  asset, and rewrites no file;
- an asset that changes on its own is picked up even though the pages around it did not
  change, and a new reference appearing inside a stylesheet is followed;
- two URLs that differ only in their query string, or in a character a naive sanitiser
  would drop, never share a path — and nothing climbs out of the archive root.

The unit tests pin the parts that are easy to get subtly wrong: the segment table's filing
and re-splitting, `RateLimiter` burst-then-throttle behaviour, the `ServerLimits` ceiling under
24 concurrent tasks, RFC 7616 Digest and MS-NLMP NTLMv2 vectors, the SPNEGO DER against the
octet layout in RFC 4178 Appendix D (independently re-parsed with `openssl asn1parse`),
`parse_proxy` on IDM's compact
`mode:value;bypass` strings, HLS/DASH manifest parsing and AES-128 decryption, project
schedule arithmetic and the name-to-file-name boundary, and — because
the browser extension is a second implementation of the same framing — the **exact
`encodeURIComponent` output** a browser produces, decoded back verbatim by the daemon.

Two more are worth naming because they are about *refusing* rather than computing: the
**checksum parser** (all four algorithms against published vectors, the bare-digest length
inference, and every malformed spec), and the **media fallback** — an MPEG-TS remux is driven
with an `ffmpeg` that is absent, one that exits non-zero, and whatever `ffmpeg` the host
actually has, and in every case publishing must succeed and the raw track must survive.

And the shutdown tests are about the process rather than the download: they start the built
binary, send it **SIGTERM** (and, as a control, SIGINT) while a trickling origin is mid-transfer,
and assert that the process exits **0** with a line saying what stopped it — a signal death has
no exit code, which is what an orchestrator reports as a crash — that the interrupted job is on
disk as `paused` with some bytes but not all of them, and that an idle daemon stops *immediately*
rather than sitting out its five-second grace. One of the four signals a foreground `riptide
download` instead of the daemon, which is the shape a one-shot job has, and asserts additionally
that nothing in the output calls the stop a failure: that command used to exit 1 for it, so a
`Restart=on-failure` unit would have restarted on every stop it asked for itself. The unit tests
around them pin the two halves of that: an idle scheduler reports idle at once, and a scheduler
with work registers a waiter that is woken by the work ending rather than by the deadline.

A third group is about the *other* platform. The layout resolver is fed a literal environment
so the Windows answers — `%LOCALAPPDATA%` for state, `%APPDATA%` for config, `%HOMEDRIVE%` joined
to `%HOMEPATH%`, an empty `XDG_DATA_HOME=`, no home at all — are asserted on Linux; the
positional-write retry loop is driven by a writer that accepts three bytes at a time, one that
accepts zero (which is an error, not a spin), and one that claims more than it was given; and the
name rules get their own cases: `NUL.txt` and `con` are devices, `COM10` is not, `movie.mp4.`
loses its dot, and a 300-character run of multi-byte characters is cut at 200 bytes without
splitting one. `scripts/check-portability.sh` then compiles those platform branches for the seven
targets they will run on — from one machine, with no cross compiler.

The extension's own framing is tested from Node (`extension/tests/protocol.test.mjs`),
including the shipped IDM sample frame `MSG#0#17#1#0:9:` and a real `HELLO_ACK` captured from
a running daemon.

## License

MIT — see [`LICENSE`](LICENSE), including the provenance note. Not affiliated with Tonec FZE.
