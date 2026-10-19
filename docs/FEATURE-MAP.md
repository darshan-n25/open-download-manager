# Feature map — Internet Download Manager ⇄ Riptide

This is the honest ledger. Every row names the IDM behaviour (with the evidence tag from
[`IDM-REVERSE-ENGINEERING.md`](IDM-REVERSE-ENGINEERING.md)), what Riptide does about it, and
where to look for the code. Rows marked **Absent** or **Refused** are the point of the
document: nothing here should be read as "IDM feature complete".

## How to read the status column

| Status | Meaning |
| --- | --- |
| **Done** | Implemented, and covered by a test that would fail if it broke. |
| **Unverified** | Implemented and unit-tested, but never exercised against real counterparty software. Treat as "probably works". |
| **Partial** | A documented subset. The row says exactly what is missing. |
| **Absent** | Not implemented. No plan implied. |
| **Refused** | Deliberately not implemented. The row says why. |

Where a row says "tested", the test is in this repository and `cargo test` runs it.

---

## 1. Transfer engine

| IDM behaviour (evidence) | Riptide | Where | Notes |
| --- | --- | --- | --- |
| Hand-rolled HTTP stack, only 7 static DLL imports; `ws2_32`/`schannel`/`secur32` loaded dynamically `[OBSERVED]` | **Done** | `src/net/client.rs`, `src/engine.rs` | `reqwest` for sockets and TLS, but every request is built, ranged, validated and retried by Riptide. TLS is `rustls` + `webpki-roots`, so no OpenSSL or `pkg-config` is needed. |
| `Range: bytes=%I64d-%I64d` formatting `[OBSERVED]` | **Done** | `src/engine.rs` `stream_range` | Same inclusive-inclusive form. |
| `Content-Range:` is trusted over local arithmetic; a shifted start is honoured `[INFERRED]` | **Done** | `src/engine.rs` `stream_range` | Logs `server shifted the range start` and rewrites the handle's `done` counter. |
| Multi-connection transfer to one URL | **Done** | `src/segment.rs`, `src/engine.rs` `run_segmented` | Test: `segmented_download_tiles_the_file_and_matches_the_source` asserts the observed ranges tile the file exactly. |
| `If-Range:` conditional resume using `ETag`/`Last-Modified` `[OBSERVED]` | **Done** | `src/engine.rs` (`validator`, `IF_RANGE`) | A changed validator makes the request fail rather than splice two different files together. |
| `Connection: Keep-Alive` `[OBSERVED]` | **Done** | `src/net/client.rs` | `reqwest`'s pool. |
| Optimistic startup: begin before the manifest is verified, leave a part file `[INFERRED]` | **Done** | `src/engine.rs` `probe`, `src/storage.rs` | A `HEAD` (falling back to `bytes=0-0`) then the real transfer. The part file is named `<output>.part` and only renamed on success. |
| **Dynamic** chunk re-splitting while running: `Change chunk size to %ld`, mutable `ChLst` `[OBSERVED]`, UI text "Turn off/on the ability of IDM to use a dynamic segmentation" `[OBSERVED]` | **Done** | `src/segment.rs` `split_off`, `src/engine.rs` `try_split` | A finishing worker halves the largest outstanding segment and hands the tail to an idle one. Re-splitting stops at `min_chunk`. |
| Retry budget per file, `Number of retries for each file if downloading failed` `[OBSERVED]` | **Done** | `job.max_retries`, `retry_backoff_ms` | Exponential-ish backoff of `backoff × attempts`. |
| Idle restarts: `Connection restarted because of timeout` `[OBSERVED]` | **Done** | `read_timeout_secs`; a timed-out read restarts the segment in single-stream mode and returns a partial in segmented mode | |
| `Max. connections number` (per file) `[OBSERVED]` | **Done** | `job.connections` | Per job. |
| `Max. connections number for a server` (per host) `[OBSERVED]` | **Done** | `src/net/limits.rs` `ServerLimits`; held across probe, single-stream, ranged and media fetches | Process-wide, shared by every job. Test: `a_per_server_ceiling_caps_parallelism_within_one_job` — 8 requested, ceiling 2, server never saw 3. |
| `Maximum download speed:` (global) `[OBSERVED]` | **Done** | `config.speed_limit`, `src/throttle.rs` | One shared token bucket. |
| `Maximum download speed for one file` (per job) `[OBSERVED]` | **Done** | `job.speed_limit`, `--limit` | |
| `multipart/byteranges` handling `[OBSERVED]` | **Absent** | — | Riptide sends exactly one range per request, so a conforming server answers with a single part. A server that returns `multipart/byteranges` anyway is not parsed. Worth adding if it ever bites. |
| `Transfer-Encoding: chunked` and non-sizeable responses `[OBSERVED]`; fall back to one streaming connection when `Accept-Ranges` is missing `[INFERRED]` | **Done** | `src/engine.rs` `run_single` | Test: `a_server_without_range_support_still_produces_correct_bytes`. |
| Compressed entities | **Partial** | `Accept-Encoding: identity` is forced on every download request | If a server compresses anyway (ignoring `identity`), the stored bytes are the compressed ones. Riptide does not decompress to disk. |
| `HEAD` before a ranged transfer | **Done** | `probe` | Falls back to `GET` with `bytes=0-0` when `HEAD` is rejected, matching IDM's tolerance of CDNs that refuse `HEAD`. |
| Content-Disposition filename selection | **Done** | `src/config.rs` output naming | |

## 2. Resume, state and integrity

| IDM behaviour | Riptide | Where | Notes |
| --- | --- | --- | --- |
| Resume after closing IDM or rebooting `[OBSERVED]` | **Done** | `src/state.rs` `JobStore` | Per-job file, written to a temp path, `fsync`ed, then renamed. A crash mid-write cannot corrupt an existing record. |
| Resume reuses already-downloaded ranges | **Done** | Test: `a_truncating_server_fails_cleanly_then_the_next_run_resumes` | Asserts the second run serves strictly fewer bytes than the whole file and still produces byte-identical output. |
| Truncated responses must not produce a corrupt file | **Done** | `Segment` state + final `all_done()` gate | The same test asserts the job fails and nothing is published as the final name. |
| Writing directly at final offsets instead of assembling at the end | **Done — and different** | `src/storage.rs` `Sink::write_at` | IDM merges parts at the end, which is its single most-complained-about behaviour. Riptide preallocates a sparse file, writes each chunk at its final offset, then `rename`s. Completion is O(1). |
| `"Change chunk size to %ld"` applied to the persisted plan | **Done** | `job.segments` | The live segment table is snapshotted into the job record on every run, so a resume continues the *current* split rather than replanning. |
| File-integrity/error detection on resume | **Done** | validator (`ETag`/`Last-Modified`) + `Content-Range` cross-check | No hashes: IDM does not publish a per-file checksum, and inventing one would need a side-channel. |

## 3. Authentication and credentials

| IDM behaviour (evidence) | Riptide | Where | Notes |
| --- | --- | --- | --- |
| `Authorization: Basic %s` `[OBSERVED]` | **Done** | `src/auth.rs` | Test `basic_header_is_rfc7617`. |
| `Authorization: Digest username="%s", realm="%s", nonce="%s", uri="%s", %sresponse="%s"` `[OBSERVED]` | **Done** | `src/auth.rs` | RFC 7616, `qop=auth`, `nc` incremented per reuse of a nonce. Test `digest_matches_rfc7616_example`. |
| `Authorization: Bearer` `[OBSERVED]` | **Done** | `src/auth.rs` | |
| `AcquireCredentialsHandleA` → SSPI, i.e. Negotiate/NTLM/Kerberos `[OBSERVED]` | **Partial** | `src/auth/ntlm.rs`, `src/auth/spnego.rs` | Both non-Kerberos mechanisms are implemented with no SSPI at all. `Negotiate` is SPNEGO (RFC 4178/4559) wrapping NTLMSSP, hand-rolled from DER up: `src/auth/spnego.rs` offers only the NTLMSSP OID in its `mechTypes` list, because RFC 4178 §3.1 forbids offering a mechanism the client cannot complete. **Kerberos is Absent** — there is no KDC client — and a server that insists on it is detected and reported, not silently failed. |
| NTLM handshake (`WWW-Authenticate: NTLM`) | **Unverified** | `src/auth/ntlm.rs` | `NTOWFv2`, Type 1/2/3 message construction (`MD4`, `HMAC-MD5`, target info, timestamp, channel-free) are unit-tested against published vectors, and the handshake now runs end-to-end in `tests/negotiate.rs` against a mock server that re-derives NTProofStr. **It has never been run against a real IIS/Exchange/SharePoint endpoint**, so expect field-ordering bugs the first time it is. |
| `WWW-Authenticate: Negotiate` (SPNEGO) `[OBSERVED]` | **Unverified** | `src/auth/spnego.rs` | Initial `initialContextToken` + `NegTokenInit` carrying an optimistic NTLMSSP Type 1; the later token is a bare `NegTokenResp` with `negState = accept-completed` (RFC 4178 §4.2 forbids re-wrapping it). The encoder is asserted against the octet layout in RFC 4178 Appendix D and independently re-parsed with `openssl asn1parse`; the decoder is checked against Kerberos-oid tokens so that case diagnoses correctly. A mock RFC 4559 server drives the whole path in `tests/negotiate.rs`. No real Windows host was available. |
| `Proxy-Authorization: Basic\|Digest` `[OBSERVED]` | **Done** | credentials embedded in the proxy URL | A `407` is reported clearly rather than retried blindly. |

## 4. Proxies and transport

| IDM behaviour (evidence) | Riptide | Where | Notes |
| --- | --- | --- | --- |
| IDM's compact proxy setting: `"1"` = direct, `"2"` = system, integer modes, `7:…` fixed servers, `;bypass,…` | **Done** | `src/net/proxy.rs` `parse_proxy` | Every documented mode plus URL forms. Entries are classified by shape (scheme + port ⇒ proxy, otherwise bypass), which is what makes `7:a;b` mean one proxy and one bypass. Tested for `"1"`, `"2"`, `"7:http://a:1;socks5://b:2"`, `;bypass`. |
| Shipped Tor presets `127.0.0.1:9050` / `127.0.0.1:9150` `[OBSERVED]` | **Done** | `parse_proxy`, `ProxySpec` | Accepts `socks5://127.0.0.1:9150` directly. |
| `socks5` proxies | **Done** | `reqwest` with the `socks` feature | |
| Proxy auto-config (WinINet `wininet` autodetect) `[OBSERVED]` | **Partial** | `src/net/proxy.rs` | A PAC **URL** is recognised and warned about, then ignored: evaluating PAC needs a JavaScript engine, which Riptide does not vendor. Explicit and system proxy settings work. |
| Per-server proxy selection, PAC-based failover | **Absent** | — | One proxy list per job, in order, no scoring. |

## 5. Queue and scheduler

| IDM behaviour (evidence) | Riptide | Where | Notes |
| --- | --- | --- | --- |
| A queue with a configurable width | **Done** | `config.max_concurrent_jobs`, `src/scheduler.rs` | A semaphore, not a thread count. |
| Start at a scheduled time (`+30m`, RFC 3339) | **Done** | `src/scheduler.rs`, `cli.rs` `parse_when` | `+30m` spans or RFC 3339 timestamps only. No cron expressions, no `cron` dependency. |
| Retry a queued file every 30 seconds until it starts `[OBSERVED, IDM tips]` | **Done** | the scheduler holds a waiting job and re-checks it | |
| Pause/resume across restarts | **Done** | `JobState::Paused` + the persisted segment table | Ctrl-C cancels via a flag so jobs land in `Paused`, i.e. resumable. |
| Shut the machine down when the queue drains `[OBSERVED]` | **Absent** | — | Deliberate: it is a platform-specific power call that a CLI should not make silently. |
| Recurring work: periodic file synchronization, and scheduled site-grabber projects | **Done** | `src/project.rs`, `riptide project`, the daemon's ticker | One persisted definition per project, `--kind file` or `--kind site`, in `state_dir/projects/`. A `file` project sends `If-None-Match`/`If-Modified-Since` and treats `304` as "nothing to do", so an unchanged sync moves no body bytes; anything else is a new version, fetched in full into the working file and renamed into place, so a failed run cannot damage the copy on disk and a stale partial from an older version is never spliced in. A `site` project re-runs the crawl and fetches only targets that changed, with the same conditional treatment per target; with `--archive` the same run keeps an offline mirror instead (see §8). The daemon runs due projects (one 30 s poll), and `riptide project tick` does the same for `cron`. Buys into the same global/per-file rate limits, per-server ceiling and quota as an interactive download. Deltas from IDM: the next run is measured from the end of the previous one, so missed slots are collapsed into a single catch-up run rather than replayed; and there is one project store rather than IDM's several separate queues. |
| Category folders (`FoldersTree`, `IDMFType.dat` extension map) `[OBSERVED]` | **Partial** | `config.auto_categorize`, `src/config.rs` | Sorts finished files into per-type subfolders using a small built-in extension map. It does **not** read IDM's `IDMFType.dat` or reproduce its folder tree. |

## 6. Media: HLS, DASH, remuxing

| IDM behaviour (evidence) | Riptide | Where | Notes |
| --- | --- | --- | --- |
| `CM3UParser` — master playlist, media playlist, media segments, variants `[OBSERVED]` | **Done** | `src/media/hls.rs` | `#EXT-X-STREAM-INF`, `#EXT-X-MEDIA`, `#EXT-X-MAP`, `#EXT-X-KEY`, `#EXT-X-BYTERANGE` (with the running-offset rule), `#EXT-X-ENDLIST`, `#EXT-X-TARGETDURATION`. |
| Variant selection by resolution/bitrate | **Done** | `select_variant` | `-q 1080` picks the closest height ≤ requested; `best`/`worst` also supported. Test `selection_best_worst_and_height`. |
| AES-128 HLS encryption with explicit or default IV `[INFERRED]` | **Done** | `src/media/hls.rs` | `METHOD=NONE` clears the key. A real bug was found here: the default-IV path once panicked on an 8-byte counter. |
| `CMp4DashReader` — DASH/ISO-BMFF `[OBSERVED]` | **Done** | `src/media/dash.rs` | `SegmentTemplate` (fixed `@duration` **and** `SegmentTimeline`), `$RepresentationID$`/`$Number$`/`$Time$`/`$Bandwidth$` with `%0Nd` formatting, `SegmentList`, `Initialization`, nested `BaseURL`, `Period` duration. |
| Prefer resolution/bitrate; separate video and audio AdaptationSets | **Done** | `Rep::is_video`/`is_audio`, `media::mux` | |
| `-tmp_dashinit`, `init_segment` temporary file handling `[OBSERVED]` | **Done** | the init segment is written first, in order, into the output | |
| `#ANVATO-SEGMENT-INFO:` Anvato support `[OBSERVED]` | **Absent** | — | An Anvato-specific sidecar convention. Not parsed. |
| `idmvconv.dll` conversion, `IDMVMPrs*.dll` parser plugins `[OBSERVED]` | **Partial** | `src/media/remux.rs` | Riptide concatenates fMP4 segments directly, and shells out to `ffmpeg` only when it must. **It does not convert codecs or containers beyond MPEG-TS → MP4.** |
| Remux MPEG-TS to MP4 | **Partial** | `src/media/remux.rs` | Needs `ffmpeg` on `PATH` (override with `RIPTIDE_FFMPEG`). Without it, the `.ts` is kept and a warning is printed rather than emitting a broken `.mp4`. **This path was never executed in development** — `ffmpeg` was not installed. |
| Muxing a separate video track with a separate audio track | **Partial** | `media::mux` | Same `ffmpeg` dependency; without it, the video track is written alone with a warning. |
| Live/`EVENT` playlists that keep growing | **Partial** | parsing stops at `#EXT-X-ENDLIST` and does not poll | A live stream is captured as far as the manifest describes at the moment of resolution. No re-polling for new segments. |

## 7. FTP

| IDM behaviour | Riptide | Where | Notes |
| --- | --- | --- | --- |
| FTP download | **Done** | `src/ftp.rs` | Passive mode only. |
| `SIZE` for the total, `REST` for resume offsets | **Done** | `src/ftp.rs` | |
| Anonymous defaults | **Done** | `anonymous` / `anonymous@` | |
| Segmented FTP (several data connections to one file) | **Absent** | — | The HTTP engine is segmented; FTP is a single control+data connection. Most FTP servers would not be happy with parallel ranged retrievals anyway. |
| `ftps://` explicit/implicit TLS | **Absent** | `ftp.rs` documents it, `parse_ftp_url` rejects it | Tests assert the refusal is explicit (`ftps_is_explicitly_unsupported`) rather than silently treating TLS as plaintext. |
| FTP traversal / `LIST`-based resume | **Partial** | the size comes from `SIZE` only | No directory listings. |

## 8. Site grabber

| IDM behaviour (evidence) | Riptide | Where | Notes |
| --- | --- | --- | --- |
| Link extraction from a page, driven out-of-process by `IDMGrHlp.exe` `[OBSERVED]` | **Done — in-process** | `src/grabber.rs` | A BFS crawl inside the daemon/CLI rather than a helper process. |
| `GrabberSts\{IncludeFilters,ExcludeFilters,Projects}`, `sprojects*.dat`, `surlexclist.dat` `[OBSERVED]` | **Partial** | `GrabberConfig` include/exclude globs, `media_only`, `src/project.rs` | Crawl filters are Riptide's own flags, and a scheduled grabber is a [`Project`](../src/project.rs) stored as JSON under `state_dir/projects/`. Neither reads IDM's `.dat` seeds, and IDM's `surlexclist.dat` (an allow-list of seed URLs) has no equivalent. |
| Depth-limited crawl | **Done** | `--depth` | Test: `the_grabber_finds_media_links_and_skips_trackers` asserts exactly one level per depth increment. |
| Page count limit | **Done** | `--max-pages` | |
| Media/archive classification with an extension map | **Done** | `classify`, plus the built-in media/archive extension sets | `.txt`/images/unknown types are intentionally not reported. |
| Tracker/ad filtering | **Done** | `is_tracker` | |
| `Get all links` / site-level "download everything" | **Partial** | `grab --download` fetches every selected target | Same machinery, no separate UI command. |
| **Complete web sites for offline browsing**, rewriting links to local files `[OBSERVED]` | **Done** | `src/archive.rs`, `grab --archive`, `project add --kind site --archive` | Fetches the page plus the stylesheets, scripts, images and fonts it needs to render, then rewrites `href`/`src`/`srcset`/`poster`/inline `url(...)`/`@import` to local relative paths. The placement and rewriting rules — and the ones deliberately not attempted — are spelled out in the two rows below. |
| Archive a site **on a schedule**, so the mirror tracks the live site | `sprojects.dat` `[OBSERVED]` | `Project.archive` | Pages and stylesheets are re-read (they are the map to everything else); every other asset is conditional, so an unchanged mirror moves no bodies and reports `unchanged`. What the server said is kept in `_riptide/validators.json` beside the mirror, so the state travels with the archive rather than with the project file. |

## 9. Browser integration

| IDM behaviour (evidence) | Riptide | Where | Notes |
| --- | --- | --- | --- |
| WebSocket on `127.0.0.1:1001`, subprotocol `plugin.v3.internetdownloadmanager.com` `[OBSERVED]` | **Done** | `src/daemon.rs` | Same address, same subprotocol, negotiated in the handshake. Test: `tests/daemon.rs` drives a real tungstenite client with the subprotocol and asserts the reply header. |
| `MSG#<id>#<type>#<a>#<b>[,k=v…];` framing; the shipped sample `MSG#0#17#1#0:9:` `[OBSERVED]` | **Done** | `encode_frame`/`parse_frame` | The shipped sample is a test fixture (`the_shipped_sample_frame_parses`). Values are percent-encoded; every delimiter inside a value survives (`delimiters_inside_values_survive`). |
| Version constant `16845059` = `0x01010903` `[OBSERVED]` | **Done** | `PROTOCOL_VERSION` | Sent in `HELLO`, echoed in `HELLO_ACK`. The daemon logs a mismatch but keeps going, so a bumped constant in a future IDM build still interoperates. |
| Opcode numbering on the wire | **Done but Riptide's own** | `msg` module | IDM's server-side opcode enum is **not** in the shipped extension (only the dispatcher ships), so the numbers are Riptide's invention and are documented as such. Framing and handshake match; numbering is not claimed to. |
| Native messaging host `com.tonec.idm` → `IDMMsgHost.exe` → named pipe `[OBSERVED]` | **Refused** | — | It needs a per-OS host manifest (or a Windows registry key), and it trusts any process that can reach the pipe. A loopback WebSocket needs neither. |
| `declarativeNetRequest` redirect rules that make the browser download through IDM `[OBSERVED]` | **Refused** | `extension/background.js` | Riptide cancels in `downloads.onCreated` instead: visible in the browser's own UI, reversible, and handed back to the browser if the daemon is down. |
| Redirecting the browser through IDM's local proxy `[OBSERVED]` | **Refused** | — | No interception of unrelated browsing and no TLS man-in-the-middle. Riptide runs its own HTTP stack. |
| Kernel filter drivers `idmwfp*.sys`, `idmtdi*.sys` `[OBSERVED]` | **Refused** | — | Kernel code, driver signing and elevated privileges to do what a userspace client already does. |
| Trust derived from the connecting process's PID `[INFERRED]` | **Refused** | — | A loopback daemon that accepts `ADD` from localhost is the honest version of the same trust model: a local process could queue downloads either way, but it cannot redirect traffic. |
| Cookies, `Referer` and `User-Agent` carried across to the download `[OBSERVED]` | **Done** | `extension/background.js` | `chrome.cookies.getAll({url})`, `downloads.onCreated`'s `referrer`, `navigator.userAgent`. |
| MIME → extension table `IDMFType.dat` `[OBSERVED]` | **Partial** | `src/engine.rs` (extension chosen from `Content-Disposition`, then `Content-Type`), `src/util.rs` `category_for_extension` | A small built-in map. It does **not** read IDM's `.dat` files. |
| HLS hand-off URL schemes (`hls2toidm://`, `idmhlstomp4v2://`, …) `[OBSERVED]` | **Absent** | — | Riptide's extension sends a normal `ADD` with the `.m3u8` URL and the daemon infers `hls`. The URL-scheme protocol exists because IDM's extension needed to launch a separate process. |
| ROT-13 site-name obfuscation (`ua("lbhghor")` = `youtube`) `[OBSERVED]` | **Refused** | — | It hides which sites are special-cased. Riptide has no per-site list to hide. |

## 10. Interface surface

| IDM behaviour (evidence) | Riptide | Where | Notes |
| --- | --- | --- | --- |
| `HKCU\Software\DownloadManager\*` configuration `[OBSERVED]` | **Done, different carrier** | `config.json` + `riptide config show/set/path/init` | Every setting is reachable from the CLI. |
| GUI with per-download properties | **Refused/Partial** | CLI + a small HTML control page on the daemon (`GET /`) | No desktop GUI. The daemon also serves a JSON API so a GUI could be written against it later. |
| Windows shell integration (context menu, IE plugin) | **Absent** | — | Linux/macOS first. |
| `.torrent` downloads | **Done** (as a file) | nothing special needed | IDM contains **no** BitTorrent protocol strings (0 hits); it only downloads the `.torrent` file. Riptide matches that. |
| Torrent/BitTorrent client | **Absent — and so is IDM's** | — | Recorded here because it is the most common misconception about IDM. |

## 11. Things Riptide does that IDM does not

- **No merge pass.** Positional writes into a preallocated sparse file, then `rename`.
- **A plain JSON HTTP API** on the same port as the extension socket (`/api/jobs`, `/health`),
  so the protocol surface is drivable and testable with `curl`.
- **A `probe` command** (`riptide probe --json`) that reports size, resumability and validators
  before committing to a download.
- **Ceilings measured in real sockets.** `ServerLimits` holds a permit for the lifetime of one
  response, so "max connections for a server" counts open connections rather than jobs started.
- **The extension has no silent browser-download rewriting.** Every takeover is visible and
  handed back to the browser on failure.
- **Cross-platform.** IDM is Windows-only.

## 12. IDM's own feature list, checked line by line

Sections 1–11 were derived from the binaries. This section goes the other way: it takes every
feature [Tonec advertises](https://www.internetdownloadmanager.com/features2.html) and checks
it, first against the shipped binary (to see whether the claim is real software and not just
marketing), then against Riptide. The point is to find the features that a
binary-first analysis never runs into because nothing in the code says "this is a feature
someone will miss".

The binary evidence column is a grep of the extracted strings of `IDMan.exe`
(`strings`-style ASCII and UTF-16 passes); "not found" means the string is absent, which is
weak evidence but the only kind available without a disassembler here.

| IDM claim | Binary evidence | Riptide | Notes |
| --- | --- | --- | --- |
| Accelerate downloads up to 8× by dynamic segmentation | `Change chunk size to %ld`, `ChLst` `[OBSERVED]` | **Done** | The core mechanism; see §1. |
| Resume interrupted downloads | resume/`Content-Range` strings `[OBSERVED]` | **Done** | §2. |
| One click: HTTP, FTP, HTTPS **and MMS** | `mms://`, `mmsPacketSize`, `mmsIndex`, `StImmMsg`, `send mms cmd`, `MMSCn:ProcRpl`, `Unk err in MMSCn:ProcWR` `[OBSERVED]` | **Absent** | IDM really does implement MMS (`MMSCn` = MMS connection). Riptide does not. MMS is a dead protocol — Windows Media Services dropped it in 2008 and `mms://` links are effectively extinct — so this is recorded as a gap rather than planned work. If you have a live MMS source, Riptide cannot fetch it. |
| All popular browsers | shipped CRX + native host `[OBSERVED]` | **Partial** | Chrome/Edge (MV3) and Firefox (an alternate manifest). No Safari, Opera, or IE. |
| Video panel: grab media from web players | `CM3UParser`, `CMp4DashReader` `[OBSERVED]` | **Partial** | HLS/DASH manifests and page `<video>` sources. No browser-side request sniffing, so a player that fetches its manifest from JavaScript needs you to paste the manifest URL. |
| Built-in scheduler: connect at a set time, download, **disconnect/hang up the modem**, or **shut the computer down** when done | `Shut down`, `Shut down now`, `Hang up modem`, `Hang up now`, `Internet Download Manager has finished queue processing. According to scheduler settings IDM is going to hang up your modem.` `[OBSERVED]` | **Partial** | Scheduled start, queue width, and recurring projects (§5) are done. Shutdown, modem hang-up, and "disconnect when done" are **Absent** — they are platform power/dial-up calls a CLI should not make silently, and dial-up is long gone. |
| **Periodic synchronization of files**; several separate queues | `Periodic synchronization`, `synchroniz*` `[OBSERVED]` | **Done** | `riptide project add --kind file`: a persisted URL↔path pair that asks the server whether the copy on disk is current (`If-None-Match`/`If-Modified-Since`) and refetches in full only when it is not. One project store rather than several separate queues — the distinction in IDM is a UI convenience, not a scheduling difference. |
| Spider/grabber: all files matching filters | `GrabberSts`, `sprojects.dat`, `surlexclist.dat` `[OBSERVED]` | **Done** | §8. |
| Grabber: **complete web sites for offline browsing**, rewriting links to local files | `Complete start page with all pictures, styles, and scripts for offline browsing`, `Convert the links in downloaded html files to the local files for offline browsing` `[OBSERVED]` | **Done** | `riptide grab --archive`, or a `site` project with `--archive`. A URL's place on disk is decided by the URL alone (host, path, query), so two URLs that differ only in their query string cannot collide and re-running overwrites rather than accumulating copies; other hosts are filed under `_hosts/<host>/`. Anything the run could not hold — filtered out, past the depth limit, or 404 — is left pointing at the **live URL** rather than at a local path that does not exist, and the run reports how many. The rewrite is deliberate regex work over attributes and `url(...)` rather than an HTML parser, so a URL built by JavaScript at run time is not seen. |
| Grabber projects on a schedule, periodically | `sprojects.dat`, `Periodic synchronization` `[OBSERVED]` | **Done** | `riptide project add --kind site`: re-runs the crawl on an interval and, with `--download`, fetches what it finds — each target getting the same conditional treatment as a `file` project, so an unchanged site costs one request per target and no bodies. With `--archive` the same run keeps an offline mirror instead. Run by the daemon when due, or by `cron` via `project tick`. |
| Download categories | `IDMFType.dat`, `FoldersTree` `[OBSERVED]` | **Partial** | §5. |
| "Download All" — add every link on the page | grabber's include/exclude filters `[OBSERVED]` | **Partial** | `riptide grab --download` does this from the CLI and from the popup's page scan. |
| **Download limits**: quotas in megabytes per hour | `Download limits`, `Download limits exceeded!` `[OBSERVED]` | **Done** | Two independent mechanisms, both from the binary's own vocabulary. **Rate limits** in bytes/second, global and per file (`Speed Limiter`, `Use Speed Limiter`, `Remember Speed Limiter settings for this file`). **A quota over a fixed window** — `quota_bytes` of `quota_window_secs`, e.g. "no more than 500 MB per hour" — enforced on every byte written, by one process-wide budget shared across jobs, the media remuxer, FTP, and crawls. Exhaustion **pauses** the job rather than failing it: progress is kept, the reason says how long until the window refills, and `riptide queue` continues it. The window is remembered in `state_dir/quota.json`, so a restart does not hand back a fresh budget. Two deliberate divergences: the window is a fixed span from the first byte charged rather than IDM's per-hour bucket, and a charge too large for the remaining budget is refused outright rather than partially written, so a job can pause with up to one chunk of budget unspent. |
| Automatic antivirus checking on completion | `If you would like to run antivirus software on download completion, you should type or browse the complete path to its executable file…`, `[file]` parameter convention `[OBSERVED]` | **Absent** | Running a configured scanner with the finished path on completion. Not implemented. |
| Proxy servers, "for example Microsoft ISA, and FTP proxy servers" | `Use &FTP Proxy` `[OBSERVED]` | **Partial** | HTTP/HTTPS/SOCKS proxies are done, including IDM's compact mode strings. An **FTP proxy** (proxying the FTP control connection) is **Absent**; FTP goes direct. |
| Auth: Basic, **Negotiate, NTLM, and Kerberos** | `AcquireCredentialsHandleA`, SSPI `[OBSERVED]` | **Partial** | §3: Negotiate is implemented as SPNEGO over NTLM, Kerberos absent, NTLMv2 unverified against a live host. |
| Drag and drop links in, files out | `Drag*` `[OBSERVED]` | **Absent** | There is no window to drag anything to. |
| Customizable interface: toolbars, skins, dark theme | `skin`, dark-theme strings `[OBSERVED]` | **Refused** | No GUI. The extension popup does follow the OS light/dark preference. |
| Multilingual (30+ languages) | `Languages*` `[OBSERVED]` | **Absent** | English only, in both the CLI and the extension. |
| Quick update: check weekly, self-update | `Update*` `[OBSERVED]` | **Absent** | No self-updater. Use `cargo install` / your package manager. |
| Simple installation wizard | `[OBSERVED]` | **Absent** | A single static binary. |
| Advanced browser integration: catch downloads from **any application** | kernel drivers `[OBSERVED]` | **Refused** | This is the filter-driver/proxy mechanism, deliberately dropped (§9). |

### What this exercise found

Four gaps that the binary-first analysis did not surface, because none of them are in the
download engine:

1. **MMS**, which is genuinely implemented and genuinely obsolete.
2. **Periodic file synchronization** and **scheduled grabber projects** — a whole
   "keep this mirror fresh" mode that Riptide has no answer for.
   *Now implemented* — see the "Recurring work" row in §5.
3. **Download quotas over a time window**, as opposed to instantaneous rate limits.
   *Now implemented* — see the "Download limits" row above.
4. **Offline site archiving with link rewriting**, as opposed to link harvesting.
   *Now implemented* — see the two new rows in §8.

Everything else on the list is either implemented, deliberately refused, or UI that a
terminal-first tool has no place for.

### What the community asks for, as opposed to what IDM ships

§1–§11 compare Riptide against **IDM**. [`COMMUNITY-REQUESTS.md`](COMMUNITY-REQUESTS.md)
compares it against what people actually ask for, which is not the same list: the most-repeated
*feature* request in the surrounding ecosystem is **BitTorrent/magnet support** (which IDM does
not have either), followed by **checksum verification**, and the most-repeated request of any
kind is simply to **run on something other than Windows**. The first two are recorded there as
gaps; the third is answered by construction.

## 13. What "tested" means here

`cargo test` runs 272 Rust tests, and `node --test extension/tests/protocol.test.mjs` runs 13
more against the extension's framing. The distinction that matters in this document:

- **Unit tests** prove an algorithm in isolation (parsers, KDFs, the segment table, the
  semaphores, the JS framing, project schedule arithmetic).
- **Protocol tests** (`tests/daemon.rs`) drive the daemon over a real WebSocket and its JSON API.
- **End-to-end tests** (`tests/segmented.rs`, `tests/projects.rs`, `tests/archive.rs`) stand up
  real HTTP servers that really do byte ranges and conditional requests, and check the bytes on
  disk; `tests/negotiate.rs` stands up a mock RFC 4559 server that issues an NTLMSSP challenge
  inside SPNEGO and re-derives the client's NTProofStr. The archive suite additionally reads its
  own output back: it re-parses every reference in every file it wrote, resolves it against that
  file's directory, and asserts the target exists.
- **Live checks** run the release binary against a separately written server on this machine —
  useful for catching build-configuration and CLI-level mistakes that in-process tests cannot
  see, but still not an interop test against anyone else's implementation. The recurring-project
  feature was checked this way end to end: a `file` project downloaded 3 MiB byte-identically,
  the second run transferred **zero** body bytes and had its `304` counted by the server, a
  changed remote replaced the file exactly even with a stale working file planted on disk, a
  `site` project fetched its targets and then skipped both on the next run, and the daemon's own
  ticker ran the project unprompted once its interval elapsed. Archiving was checked the same
  way against the same separately written conditional-`GET` server, and the resulting mirror was
  opened from disk with the server stopped.

Nothing in this repository has been run against: a real Kerberos realm, an NTLM-protected
Windows host, a PAC file, an FTPS server, `ffmpeg` (a build is present on this host now, but it
segfaults on every MPEG-TS input, so the remux path is *still* unverified — the reason changed,
not the verdict; see [`ROADMAP.md`](ROADMAP.md)), an MMS source, a live HLS/DASH origin, a
scheduler other than a self-written `cron`-shaped one, or a real web server other than the ones
written for these tests. Those rows are labelled **Unverified**, **Partial**, **Absent** or
**Refused** above and should be treated that way. The mock `Negotiate` server in
`tests/negotiate.rs` and the conditional-`GET` server in `tests/projects.rs` were written from
the specifications too, so agreement with them proves conformance to those documents, not to
IIS or to nginx.

## 14. Sources

- `docs/IDM-REVERSE-ENGINEERING.md` — the binary analysis this project was designed from. Every
  behavioural claim above tagged `[OBSERVED]` comes from there or from a `strings` pass over
  `IDMan.exe` (`/tmp/opencode/idm/ascii.txt`, `wide.txt`).
- `https://www.internetdownloadmanager.com/features2.html` — Tonec's own feature list, used as
  the checklist in §12. Marketing copy: read as claims to be verified, never as specifications.
- IDM's shipped browser extension and message host, which define the wire protocol in §9.
