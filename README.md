# Riptide

A cross-platform download manager in the spirit of Internet Download Manager: parallel
ranged connections, crash-safe resume, a scheduler, HLS/DASH capture, FTP, browser
integration, a link grabber, and offline site archiving.

Riptide is a **clean-room reimplementation**. It was designed by statically reverse
engineering the shipped IDM binaries and then rebuilding the *behaviour* in Rust — no IDM
code is used or present. The analysis lives in
[`docs/IDM-REVERSE-ENGINEERING.md`](docs/IDM-REVERSE-ENGINEERING.md) (every claim tagged
`OBSERVED` / `INFERRED` / `UNKNOWN`) and the feature-by-feature comparison, including what is
**not** implemented, lives in [`docs/FEATURE-MAP.md`](docs/FEATURE-MAP.md).

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
| HTTP/HTTPS | Segmented ranged download, resume with `If-Range` validators, retries with backoff, redirects, cookies, custom headers, `Content-Disposition` filename handling |
| Auth | Basic, Digest (RFC 7616), Bearer, NTLMv2 (MS-NLMP written from scratch), and Negotiate/SPNEGO (RFC 4178/4559) wrapping NTLM — real Kerberos is not implemented |
| Proxies | `http`, `https`, `socks5`, IDM's compact `mode:value;bypass` strings, Tor presets |
| Queue | Persistent job store, configurable width, scheduled starts, pause/resume across restarts |
| Limits | Global and per-file rate limits, a per-server connection cap, and a bytes-per-window budget (`--quota 500M --quota-window 1h`) that pauses jobs instead of failing them |
| Media | HLS (`EXT-X-*`, byteranges, AES-128 keys, master playlists) and DASH (SegmentTemplate, SegmentTimeline, SegmentList); separate audio+video tracks are muxed into one file |
| FTP | Passive mode, `SIZE`, `REST`-based resume, anonymous defaults |
| Grabber | BFS crawl with depth/page limits, include/exclude wildcards, media/archive classification, tracker filtering |
| Archiving | `grab --archive` (or a `site` project with `--archive`) keeps a page and everything it needs to render — stylesheets, scripts, images, fonts — rewriting the markup to local relative paths; every URL gets a path decided by the URL alone, and anything it could not hold keeps pointing at the live site |
| Projects | Recurring work stored as one document per project: `file` keeps a URL in step with a local path using conditional requests (`304` costs no body bytes), `site` re-crawls and fetches only what changed, `site --archive` keeps an offline mirror; run by the daemon when due, or from `cron` with `project tick` |
| Browser | MV3 extension + a loopback daemon speaking IDM's `MSG#` framing over its WebSocket subprotocol, plus a plain JSON API |
| CLI | `download`, `grab`, `daemon`, `probe`, `list`, `show`, `remove`, `pause`, `queue`, `project`, `config`, `state-dir` |

## Build and install

Needs a recent stable Rust toolchain (developed against 1.99). There is no `cmake`,
`pkg-config` or OpenSSL requirement: TLS is `rustls` with `webpki-roots`.

```sh
cargo build --release
install -m755 target/release/riptide ~/.local/bin/riptide
cargo test         # 272 tests: unit + protocol + end-to-end
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
curl -s -X POST http://127.0.0.1:1001/api/projects/feed/run   # run a project now
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
  quota.rs       bytes-per-window budget ("Download limits"), persisted across restarts
  scheduler.rs   queue width, scheduled starts, cancellation propagation
  auth.rs        Basic, Digest, Bearer; spec parsing and challenge handling
  auth/ntlm.rs   NTLMv2 (NTOWFv2, Type 1/2/3 messages) from MS-NLMP
  auth/spnego.rs SPNEGO (RFC 4178/4559): hand-rolled DER, wrapping, mechanism classification
  net/           client construction, TLS policy, proxy parsing and resolution
  media/         HLS resolver, MPD parser, muxing and remuxing
  grabber.rs     link extraction, filtering, crawl
  archive.rs     offline mirroring: URL→path placement, markup rewriting, asset fetching
  project.rs     recurring projects: periodic file sync, scheduled site crawls, due-time logic
  ftp.rs         passive-mode FTP with REST resume
  daemon.rs      `MSG#` WebSocket service + JSON HTTP API + project ticker
extension/       MV3 browser integration (background, popup, options)
tests/           end-to-end tests against servers that really do byte ranges, conditional requests and SPNEGO, plus daemon protocol tests and an archive suite that reads its own output back
```

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
| Native messaging host (`IDMMsgHost.exe`) trusted by PID | WebSocket on loopback | No registry keys or per-OS host manifests; the peer is a real endpoint |
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

**Media**

- **MPEG-TS remux needs `ffmpeg` on `PATH`.** Without it the `.ts` segments are kept and a
  warning is printed, rather than emitting a broken `.mp4`. Set `RIPTIDE_FFMPEG` to a specific
  binary to override the lookup. This path has never been executed here.
- **Live HLS playlists are not re-polled** — a capture covers the manifest as it stood when it
  was read.
- **No live-stream sniffing.** A player that fetches its manifest from JavaScript will not be
  detected; paste the manifest URL, or let the extension's page scan find it.

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
- **No BitTorrent.** IDM has none either; it only downloads `.torrent` files.

**Platform and interface**

- **No Windows shell integration** (IE/context-menu hooks). The CLI and the extension are the
  interfaces.
- **No GUI, so no skins, drag-and-drop, or shutdown/hang-up-when-done scheduler actions.**
- HTTP/2 is requested but **multiplexing is not exploited**: each range is still its own
  request stream, and the connection count is what drives the speedup.

## Testing

```sh
cargo test                              # Rust: 272 tests
node --test extension/tests/protocol.test.mjs   # JavaScript: 13 tests
```

272 Rust tests: 220 unit tests across every module, 10 daemon-protocol tests driving a real
WebSocket client against a bound daemon, 9 end-to-end tests that stand up an HTTP server
which really does byte ranges, 6 that stand up a mock `Negotiate` server, 17 that stand up a
server which really honours `If-None-Match` and `Range`, and 10 that archive a small site and
then read their own output back to check that every rewritten reference resolves.

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

The extension's own framing is tested from Node (`extension/tests/protocol.test.mjs`),
including the shipped IDM sample frame `MSG#0#17#1#0:9:` and a real `HELLO_ACK` captured from
a running daemon.

## License

MIT — see [`LICENSE`](LICENSE), including the provenance note. Not affiliated with Tonec FZE.
