# Riptide

A cross-platform download manager in the spirit of Internet Download Manager: parallel
ranged connections, crash-safe resume, a scheduler, HLS/DASH capture, FTP, browser
integration, and a link grabber.

Riptide is a **clean-room reimplementation**. It was designed by statically reverse
engineering the shipped IDM binaries and then rebuilding the *behaviour* in Rust — no IDM
code is used or present. The analysis lives in
[`docs/IDM-REVERSE-ENGINEERING.md`](docs/IDM-REVERSE-ENGINEERING.md) (every claim tagged
`OBSERVED` / `INFERRED` / `UNKNOWN`) and the feature-by-feature comparison, including what is
**not** implemented, lives in [`docs/FEATURE-MAP.md`](docs/FEATURE-MAP.md).

> **Read the feature map before trusting a checkbox.** Kerberos/Negotiate, PAC script
> evaluation, FTPS, MMS and MPEG-TS remuxing without `ffmpeg` are honest gaps, and NTLM has
> not been tested against a real server. §12 of that document checks IDM's own advertised
> feature list line by line and names four whole features Riptide has no answer for —
> periodic file synchronization, scheduled grabber projects, download quotas, and offline
> site archiving.

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
- **Independent limits.** A global ceiling, a per-file ceiling, and a per-server cap, exactly
  as IDM's settings expose them.
- **No end-of-download merge.** IDM assembles parts at the end. Riptide writes every chunk at
  its final offset into a preallocated sparse file and finishes with a `rename`, so the cost of
  finishing is O(1) and the file is never in a half-merged state.

## Feature overview

| Area | What you get |
| --- | --- |
| HTTP/HTTPS | Segmented ranged download, resume with `If-Range` validators, retries with backoff, redirects, cookies, custom headers, `Content-Disposition` filename handling |
| Auth | Basic, Digest (RFC 7616), Bearer, NTLMv2 (RFC/MS-NLMP implemented from scratch — see gaps) |
| Proxies | `http`, `https`, `socks5`, IDM's compact `mode:value;bypass` strings, Tor presets |
| Queue | Persistent job store, configurable width, scheduled starts, pause/resume across restarts |
| Media | HLS (`EXT-X-*`, byteranges, AES-128 keys, master playlists) and DASH (SegmentTemplate, SegmentTimeline, SegmentList); separate audio+video tracks are muxed into one file |
| FTP | Passive mode, `SIZE`, `REST`-based resume, anonymous defaults |
| Grabber | BFS crawl with depth/page limits, include/exclude wildcards, media/archive classification, tracker filtering |
| Browser | MV3 extension + a loopback daemon speaking IDM's `MSG#` framing over its WebSocket subprotocol, plus a plain JSON API |
| CLI | `download`, `grab`, `daemon`, `probe`, `list`, `show`, `remove`, `pause`, `queue`, `config`, `state-dir` |

## Build and install

Needs a recent stable Rust toolchain (developed against 1.99). There is no `cmake`,
`pkg-config` or OpenSSL requirement: TLS is `rustls` with `webpki-roots`.

```sh
cargo build --release
install -m755 target/release/riptide ~/.local/bin/riptide
cargo test         # 152 tests: unit + protocol + end-to-end
```

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

# Capture a stream, preferring 1080p, from a manifest URL.
riptide download https://cdn.example.com/master.m3u8 -q 1080

# Find what a page offers, then fetch it.
riptide grab https://example.com/downloads --depth 2 --include '*.zip' --include '*.iso'
riptide grab https://example.com/downloads --download --output-dir ~/Downloads

# See the queue.
riptide list
riptide show 3f1a9c02
```

`riptide` keeps its configuration and job state under your platform's config/data
directories; `riptide config path` and `riptide state-dir` print the exact locations.
Every setting is editable from the CLI:

```sh
riptide config show
riptide config set connections 16
riptide config set speed_limit '"4M"'
```

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
curl -s -X POST http://127.0.0.1:1001/api/jobs \
     -H 'content-type: application/json' \
     -d '{"url":"https://example.com/big.iso","connections":16}'
```

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
  state.rs       crash-safe job store (temp file + fsync + rename per job)
  throttle.rs    global / per-file token-bucket rate limiting
  scheduler.rs   queue width, scheduled starts, cancellation propagation
  auth.rs        Basic, Digest, Bearer; spec parsing and challenge handling
  auth/ntlm.rs   NTLMv2 (NTOWFv2, Type 1/2/3 messages) from MS-NLMP
  net/           client construction, TLS policy, proxy parsing and resolution
  media/         HLS resolver, MPD parser, muxing and remuxing
  grabber.rs     link extraction, filtering, crawl
  ftp.rs         passive-mode FTP with REST resume
  daemon.rs      `MSG#` WebSocket service + JSON HTTP API
extension/       MV3 browser integration (background, popup, options)
tests/           end-to-end tests against a real range-serving HTTP server, and daemon protocol tests
```

## How Riptide differs from IDM

Some of IDM's mechanisms are load-bearing for its business model and not for downloading.
Riptide keeps the parts that make transfers fast and drops the parts that exist to own the
browser.

**Kept, because they are the actual engineering:**

- A hand-rolled transfer path with `Range`/`If-Range`/`Content-Range` handling rather than a
  library that hides the primitives.
- A mutable chunk table with runtime re-splitting.
- Independent global, per-file and per-server limits.
- A thin browser extension paired with a privileged local daemon: the browser supplies session
  state, the daemon does the work.

**Dropped, deliberately:**

| IDM | Riptide | Reason |
| --- | --- | --- |
| Kernel filter drivers (`idmwfp*.sys`, `idmtdi*.sys`) | None | Needs kernel code, driver signing and admin rights to do what a userspace proxy client already does |
| Redirects browser traffic through IDM's proxy | Nothing — Riptide runs its own HTTP stack | No interception of unrelated browsing, and no TLS man-in-the-middle requirement |
| `declarativeNetRequest` rules to silently rewrite downloads | `downloads.onCreated` + explicit cancel | Visible, reversible, and hands the download back to the browser if the daemon is down |
| Native messaging host (`IDMMsgHost.exe`) trusted by PID | WebSocket on loopback | No registry keys or per-OS host manifests; the peer is a real endpoint |
| Merge-based completion | Direct positional writes + `rename` | The merge step is the single most common IDM complaint |

## Known limitations

Tracked in full in [`docs/FEATURE-MAP.md`](docs/FEATURE-MAP.md) — including a line-by-line
check of [Tonec's own feature list](https://www.internetdownloadmanager.com/features2.html)
against this implementation (§12 there). The short version:

**Protocol and security**

- **Kerberos/Negotiate** is not implemented. Only Basic/Digest/Bearer/NTLMv2 are.
- **NTLMv2 is unverified against a live server** — the message layout and hash are tested
  against published vectors, but no interop run has happened.
- **PAC scripts are not evaluated**: no in-process JavaScript engine. A PAC URL logs a warning
  and falls back to the ambient proxy configuration.
- **FTP is single-connection**, `ftps://` is explicitly refused rather than silently treated as
  plaintext, and an **FTP proxy** is not supported.
- **MMS is not supported.** IDM really implements it (`MMSCn` in its own strings), but the
  protocol was dropped by Windows Media Services in 2008 and `mms://` links are effectively
  extinct.
- **`multipart/byteranges` responses are not parsed** (Riptide always asks for one range per
  request, so a conforming server never sends them).

**Media**

- **MPEG-TS remux needs `ffmpeg` on `PATH`.** Without it the `.ts` segments are kept and a
  warning is printed, rather than emitting a broken `.mp4`. Set `RIPTIDE_FFMPEG` to a specific
  binary to override the lookup. This path has never been executed here.
- **Live HLS playlists are not re-polled** — a capture covers the manifest as it stood when it
  was read.
- **No live-stream sniffing.** A player that fetches its manifest from JavaScript will not be
  detected; paste the manifest URL, or let the extension's page scan find it.

**Whole features IDM has that Riptide does not**

- **No periodic file synchronization** and **no scheduled grabber projects** — the "keep this
  mirror in step" mode. Riptide downloads what you ask it to, once.
- **No download quota over a time window** ("stop after 500 MB this hour"). Rate limits
  (bytes/second, global and per file) are implemented; quotas are not.
- **No offline site archiving.** The grabber finds and downloads matching targets; it does not
  fetch a page's CSS/JS/images and rewrite its links for offline browsing.
- **No antivirus-on-completion hook**, no self-updater, no installation wizard, and no UI
  languages other than English.
- **No BitTorrent.** IDM has none either; it only downloads `.torrent` files.

**Platform and interface**

- **No Windows shell integration** (IE/context-menu hooks). The CLI and the extension are the
  interfaces.
- **No GUI, so no skins, drag-and-drop, or shutdown/hang-up-when-done scheduler actions.**
- HTTP/2 is requested but **multiplexing is not exploited**: each range is still its own
  request stream, and the connection count is what drives the speedup.

## Testing

```sh
cargo test                              # Rust: 152 tests
node --test extension/tests/protocol.test.mjs   # JavaScript: 13 tests
```

152 Rust tests: 138 unit tests across every module, 7 daemon-protocol tests driving a real
WebSocket client against a bound daemon, and 7 end-to-end tests that stand up an HTTP server
which really does byte ranges.

The end-to-end ones assert that

- the ranged requests **tile the file exactly** — no gap, no overlap, more than one request;
- a server that truncates responses produces a *failed* job and **no** file published as final;
- the next run **reuses the partial data** and produces a byte-identical file, serving strictly
  fewer bytes than the whole file;
- a server that refuses ranges still yields correct bytes in single-stream mode;
- the **per-server ceiling** is honoured: a job that asks for 8 connections while the ceiling is
  2 never makes the server see more than 2 at once, and still uses both slots;
- the grabber follows exactly one level per `depth` and filters trackers.

The unit tests pin the parts that are easy to get subtly wrong: the segment table's filing
and re-splitting, `RateLimiter` burst-then-throttle behaviour, the `ServerLimits` ceiling under
24 concurrent tasks, RFC 7616 Digest and MS-NLMP NTLMv2 vectors, `parse_proxy` on IDM's compact
`mode:value;bypass` strings, HLS/DASH manifest parsing and AES-128 decryption, and — because
the browser extension is a second implementation of the same framing — the **exact
`encodeURIComponent` output** a browser produces, decoded back verbatim by the daemon.

The extension's own framing is tested from Node (`extension/tests/protocol.test.mjs`),
including the shipped IDM sample frame `MSG#0#17#1#0:9:` and a real `HELLO_ACK` captured from
a running daemon.

## License

MIT — see [`LICENSE`](LICENSE), including the provenance note. Not affiliated with Tonec FZE.
