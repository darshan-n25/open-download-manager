# What the IDM community asks for

A survey of what users of Internet Download Manager ask for, and what people looking for an
IDM alternative ask for instead — with Riptide's current answer to each.

This is the community-facing counterpart to
[`IDM-REVERSE-ENGINEERING.md`](IDM-REVERSE-ENGINEERING.md) (what the shipped binary does) and
[`FEATURE-MAP.md`](FEATURE-MAP.md) (feature-by-feature comparison). Those two are built on
artifacts this machine can hold in its hand: the binary, its strings, its companion files. This
one is built on *reading what people write*, which is a weaker kind of evidence and is treated
as such below.

## How this was gathered, and what it is not

**Used:** IDM's own feature page, news page and FAQ (`internetdownloadmanager.com`); the
feature strings inside the shipped binary (see the reverse-engineering document, §8b); public
discussion reachable from a plain HTTPS client; and issue trackers of adjacent open-source
download managers, which are useful because they are the *same audience* asking for the same
things from a project that reads its own issues.

**Could not use:** Reddit's web UI and its JSON API both refuse this client (`403`, and a
JavaScript gate on the mirrors), and the domain `forum.internetdownloadmanager.com` now
redirects to the marketing site — IDM has no public suggestion board to read. So Reddit
material appears here as **search-result snippets** (the DuckDuckGo HTML endpoint), not as
pages read in full. **Treat quoted wording as indicative and the themes as the reliable part.**
Each claim below says which it is.

**Not:** a claim about IDM's licence, pricing or legality. Several of the most-visited threads
on these topics are about trial resets and cracked builds; that is noted only where it explains
a *product* request (people dislike being nagged), never endorsed or reproduced.

## 1. Platform reach — the loudest request by far

IDM is Windows-only, and says so itself:

> "The current version of IDM does not support Mac, IPad/IPhone, Android or Unix/Linux
> operat[ing] systems. Maybe we will release compatible versions in the future."
> — IDM FAQ, `internetdownloadmanager.com/register/new_faq/functions2.html` *(read in full)*

What users do with that:

* Linux switchers ask for "a download manager as powerful as IDM" and say the alternatives they
  tried were not handy or too heavy (r/linux4noobs, "Internet Download Manager (IDM) alternative
  for Linux") *(snippet)*.
* macOS switchers ask for "a Mac download manager similar to IDM", specifically naming Safari
  integration and "the download video grabber button in the top corner when playing YouTube
  videos" (r/MacOS, "Download Manager?") *(snippet)*.
* The workaround is Wine, and the guides for it concede the cost: installing IDM on Linux works,
  but "browser integration limitations and stability concerns make it less ideal for daily Linux
  use" (third-party guide, 2026) *(read in full)*. Several GitHub projects exist purely to
  automate that Wine setup.

**Riptide:** answered by construction. Rust, first-party Linux/macOS/Windows binaries, no Wine,
no filter driver — the integration point is a loopback daemon, which is a normal thing to run on
any of the three.

## 2. Browser integration that survives the browser changing

The integration is the product for most people, and it is also where the most visible breakage
happens:

* Chrome disabled IDM's extension along with other Manifest V2 extensions (r/software, "Chrome
  has disabled Internet Download Manager (IDM) along with Ublock Origin.. so what now?")
  *(snippet)*.
* IDM's own instruction is that the extension must always be installed and enabled, "if you have
  any problem, first check that 'IDM integration module' extension is installed and enabled" —
  i.e. the failure mode users are told to expect is the integration silently not being there
  *(IDM front page, read in full)*.
* Firefox users have asked Mozilla to build an IDM-like manager into the browser instead
  ("Feature Request: Integrated Download Manager like IDM", connect.mozilla.org) — the request
  lists pause/resume, batch download and multi-connection as the wants *(snippet)*.

**Riptide:** the daemon is the integration, and the extension is a thin client — MV3, no native
messaging host, no PID-based trust, and no monkeying with the browser's proxy settings. The
practical difference: everything the extension can do, `curl http://127.0.0.1:1001/api/...` and
the CLI can do too, so a browser policy change degrades the *convenience* rather than the
capability. See `docs/FEATURE-MAP.md` §9 for what the extension protocol owes to IDM's.

## 3. Media grabbing is why people stay — and where it fails

The video panel is the feature people name when asked why they use IDM: "the integration with
browsers and video sites is great … even the small button to download videos came very handy"
(r/Piracy) *(snippet)*. When it breaks, it breaks loudly:

* Protected/DRM streams: "IDM Cannot Download This Protected Stream" is a recurring support
  topic, and the honest answer is that Widevine-style DRM is doing its job *(third-party
  write-up, snippet)*.
* YouTube specifically: a genre of guides exists just for "IDM not showing the download option /
  not detecting videos after a YouTube change" *(search results, not an IDM statement)*.

**Riptide:** HLS and DASH manifests are first-class — `EXT-X-*` including byteranges and AES-128
keys, `SegmentTemplate`/`SegmentTimeline`/`SegmentList`, and separate audio+video renditions
muxed into one file. *No* DRM circumvention, ever: a Widevine-protected stream is not a download
manager's problem to solve, and Riptide says so rather than failing obscurely. Nor is there a
per-site YouTube layer: Riptide fetches manifests it is given, and does not implement signature
cipher recovery.

## 4. Speed, resume, batch

The oldest asks, and the ones that still lead the pitch: "increase download speeds by up to 8
times", "paste like 100 link at the same time and all of them will be downloaded", "for a file
of 1 Gb I'll download it in 30 min with a normal browser and 10 min with IDM" (IDM front page;
r/Piracy) *(both snippets/read in full)*.

**Riptide:** dynamic segmented transfer with runtime re-splitting, `If-Range` conditional resume,
crash-safe state next to the output file, a persistent queue, and per-server ceilings. Batch is
the queue plus `riptide queue`/the daemon API; there is no "paste 100 URLs" *dialog*, because
pasting 100 URLs into an argument list is what a shell already does.

## 5. BitTorrent and magnet links — the request that keeps a second app installed

This is the most-repeated *feature* request around IDM, and it is usually phrased as the last
missing piece:

> "Support for downloading BitTorrent and Magnet links directly would make AB Download Manager
> the absolute king of download managers. It's the only thing keeping some of us tied to other
> apps like FDM or Gopeed." — ab-download-manager issue #829 *(snippet)*

The alternatives market torrents as the differentiator: Free Download Manager (Windows/Linux/
macOS/Android + torrents), Rayburst (HTTP/HTTPS/FTP/**torrents**/ED2K/magnet), Plexo ("files and
torrents") *(snippets)*. And a widely shared piece frames the problem as too many tools: "one app
for torrents, a separate client for magnet links, and the browser fumbling through everything
else … I had four tools doing the job of one" (MUO) *(snippet)*.

**Not in IDM either.** IDM downloads `.torrent` *files*; it does not speak BitTorrent. So this is
a request from the wider audience rather than something IDM users get today.

**Riptide:** absent, deliberately. A BitTorrent client is a second engine — peer wire protocol,
DHT, piece selection, seeding policy, its own scheduler — and bolting it onto a segmented HTTP
engine would make both worse. If it ever happens it will be a separate binary sharing the queue
and the state store, not a mode of `download`.

## 6. Integrity checking after the bytes land

Quietly one of the most-requested *small* features in the alternative ecosystem: "SHA-256
checks" is listed as a headline capability by Flux Download Manager, and aria2c's feature set is
routinely summarised as "BitTorrent/magnet, **checksum verification**, RPC daemon mode"
*(snippets)*.

**Riptide:** absent. This is the cheapest real gap on this page — a `--checksum sha256:…` that
verifies before the final rename fits the existing "write to a temp, then rename" path exactly,
and would turn a silently corrupted download into a failed one.

## 7. Headless, remote, and automation

Requests cluster around "run it somewhere that is not my desktop, and talk to it":

* NAS/headless servers are a headline feature for newer projects ("headless NAS server, MCP /
  REST / aria2 APIs" — RinaDown) *(snippet)*.
* aria2c's "RPC daemon mode" is treated as a defining feature *(snippet)*.
* IDM's own answer is the scheduler plus periodic synchronization, which is a desktop-shaped
  answer to a server-shaped question (see §9).

**Riptide:** the daemon, its JSON API, the CLI, and `project tick` for cron are the closest match
on this page — you can run the daemon on a NAS and drive it over HTTP, with an optional bearer
token required on every route (HTTP and the WebSocket handshake alike) so binding it beyond
loopback is a decision rather than an accident. One honest gap remains: the only UI is a status
page. That is cheap; it is not done.

## 8. No ads, no nag, no account

The most common framing of *why* people look for an alternative is not a missing feature at all:
"free, open source (MIT), and has **no ads and no account**" (Prism, dev.to); "zero-ads,
zero-tracking" (FluxDown, Medium); alternativeto's "open source" filter on the IDM page
*(snippets)*. The flip side is the volume of discussion about IDM's registration nag — the "fake
serial detected" popup, trial resets, and wanting the popups to stop (r/Piracy threads)
*(snippets)*.

**Riptide:** none of it. No account, no telemetry, no self-updater, no installer, no nag. That
is not a feature to build; it is a feature to *not build*, and it is the one place where the
design brief and the community's most emotional request agree for free.

## 9. Scheduling that behaves like a schedule

IDM's scheduler and "periodic synchronization of files" are recurring answers in threads about
keeping a local copy fresh *(IDM front page, read in full)*. The complaint that follows is
usually that a missed schedule is either replayed in a burst or silently skipped.

**Riptide:** `project` does file sync and scheduled site crawls, with the deliberate difference
documented in `FEATURE_MAP.md` §5: the next run is measured from the **end** of the previous one,
so downtime collapses missed slots into one catch-up run instead of replaying them. Archiving
(§10) rides on the same runner.

## 10. Offline copies of sites

IDM advertises "complete start page with all pictures, styles, and scripts for offline browsing"
and "convert the links in downloaded html files to the local files", and the binary carries the
matching strings (`[OBSERVED]`, see §8b of the reverse-engineering document).

**Riptide:** `grab --archive` and `site --archive` projects, with the placement and rewriting
rules written down in `README.md`. See `FEATURE_MAP.md` §8.

## What is left, ranked by what this survey suggests

Not commitments — candidates, ordered by (evidence × cost):

1. **Checksum verification** (§6). Small, self-contained, asked for across projects, and it
   slots into a code path that already exists.
2. **Daemon authentication** (§7). A bearer token on the loopback API would let the daemon
   listen on a LAN address, which is what the NAS users are actually asking for.
3. **A headless-friendly release story** (§1, §7): static Linux/macOS binaries and a container
   image. Nothing to write; everything to package.
4. **BitTorrent as a sibling binary** (§5). The demand is real and it is the single reason
   people cite for keeping a second app. It is also a project-sized piece of work, and the
   answer to "should this be part of the segmented HTTP engine" is no.
5. **Not** DRM circumvention, and **not** a per-site media extractor (§3): the first is a
   different kind of software, and the second is a maintenance treadmill that IDM itself
   visibly struggles with.

Since this list was written, candidates 1 and 2 have shipped: checksum verification, and daemon
authentication (a constant-time bearer token from a file, the environment, or a config path,
required on every HTTP route and on the WebSocket handshake). See [`ROADMAP.md`](ROADMAP.md) for
what is left and the order it is ranked in.

## Sources

* `internetdownloadmanager.com` — front page, `/news.html`, `/features2.html`, and the
  registration FAQ pages (Unix/Linux/macOS answer, browser integration, video players, sharing
  sites).
* `forum.internetdownloadmanager.com` — redirected to the marketing site; no public suggestion
  board exists.
* Search-result snippets from the DuckDuckGo HTML endpoint for: IDM Linux/macOS alternatives;
  IDM YouTube and protected-stream problems; Chrome disabling the IDM extension; IDM "fake
  serial" nag threads; open-source IDM alternatives; torrent/checksum/RSS/daemon feature
  requests in adjacent download managers.
* Project pages quoted for feature expectations: AB Download Manager (issues), Flux Download
  Manager, Prism, FluxDown, RinaDown, Free Download Manager, Rayburst, Plexo, qBittorrent,
  aria2c.
* Reddit threads named above (r/linux4noobs, r/MacOS, r/software, r/Piracy, r/opensource,
  r/freesoftware, r/DataHoarder) — read as snippets, since the site refuses a non-browser
  client.
