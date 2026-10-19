# Riptide — Verification Log and Roadmap

Two things live here:

1. **A verification log.** Four limitations were carried in [`FEATURE-MAP.md`](FEATURE-MAP.md)
   §13 and the README. On 2026-10-19 each was re-checked against the working tree and the
   host. One premise had changed (ffmpeg), one conclusion had changed (a disassembler is now
   available), and two held. The verdicts and the raw evidence are below, so the claims can be
   trusted or re-run rather than taken on faith.
2. **A ranked roadmap** for what to build next, including the requests the
   [community survey](COMMUNITY-REQUESTS.md) surfaced.
3. **What has shipped from it.** Items 1 (checksum verification), 2 (daemon authentication) and 6
   (MPEG-TS remux hardening) were all built on 2026-10-19; their entries in the roadmap table
   carry a status, and a section each describes what actually landed and how it is tested.

Every claim here is tagged the same way the reverse-engineering report tags its own:
`OBSERVED` (a command was run, here is its output), `INFERRED`, or `UNKNOWN`.

---

## Part 1 — The four standing claims

### Claim 1 — “Negotiate is conformance-verified, not interop-verified”

> *No real Kerberos realm, IIS/NTLM host, FTPS server, PAC file, or live HLS/DASH origin was
> available. Negotiate is proven against a mock that re-derives the client’s NTProofStr.*

**Verdict: OBSERVED — confirmed, and the exact surface is now enumerated.**

| Sub-claim | Evidence |
| --- | --- |
| The Negotiate server is a hand-rolled mock | `tests/negotiate.rs:52` `struct MockServer`, `:66` `TcpListener::bind("127.0.0.1:0")`, four modes (`Spnego`, `KerberosOnly`, `BareNtlmOnly`, wrong-password) driven by one implementation |
| FTPS is refused, not attempted | `src/ftp.rs:37` returns *“FTPS (FTP over TLS) is not implemented; use plain ftp:// for now”*; asserted by `ftps_is_explicitly_unsupported` (`src/ftp.rs:412`) |
| PAC is parsed but never evaluated | `src/net/client.rs:61` — the only consumer of `ProxySpec::Pac`. A plain proxy URL (`…://…`, not `*.pac`) is honoured; a real `.pac` script logs *“PAC script evaluation is not supported; using ambient proxy settings”* and falls back |
| No media origin is ever contacted | `tests/` holds `archive.rs`, `daemon.rs`, `negotiate.rs`, `projects.rs`, `segmented.rs` — there is **no** HLS/DASH end-to-end test. The media module is covered by unit tests only |

**Consequence (unchanged):** agreement with `tests/negotiate.rs` proves conformance to
RFC 4559 and MS-NLMP. It does **not** prove authentication against Microsoft’s stack, and
Kerberos proper remains unimplemented (detected and reported, never silently failed).

### Claim 2 — “ffmpeg is not installed, so the MPEG-TS remux path has never run”

> *ffmpeg is not installed, so the MPEG-TS remux path has never actually executed. fMP4
> concatenation is covered; TS falls back to keeping `.ts` with a warning.*

**Verdict: the premise is now FALSE, the conclusion still HOLDS — for a new and more
interesting reason.**

The premise first: ffmpeg **is** installed.

```console
$ command -v ffmpeg && ffmpeg -version | head -1
/home/admin/.local/bin/ffmpeg
ffmpeg version 7.0.2-static https://johnvansickle.com/ffmpeg/  Copyright (c) 2000-2024
```

So the path was exercised for the first time — and it cannot be, because this particular build
**cannot demux MPEG-TS at all**:

```console
$ ffmpeg -hide_banner -y -f lavfi -i "testsrc=duration=2:size=320x240:rate=15" \
         -f lavfi -i "sine=frequency=440:duration=2" -c:v mpeg2video -c:a aac -f mpegts t.ts
$ ls -l t.ts
-rw-r--r-- 1 admin admin 92308 t.ts          # writing TS works
$ ffmpeg  -i t.ts -c copy out.mp4   ; echo exit=$?
Segmentation fault
exit=139                                     # reading TS crashes
$ ffprobe t.ts                      ; echo exit=$?
exit=139                                     # …and so does probing it
$ ffmpeg  -i b.mp4 -f null -        ; echo exit=$?
exit=0                                       # reading MP4 is fine
```

The file is a valid MPEG-TS (sync byte `0x47`, service name `FFmpeg Service01`), produced by
the same binary that then fails to open it. The crash is in the **mpegts demuxer**, not in
Riptide and not in the file.

Two consequences, both now roadmap items:

- **The remux path still cannot be verified end to end.** Not for want of ffmpeg, but because
  the only ffmpeg present is broken on its input format. `remux_ts()` runs ffmpeg twice (with
  and without `-bsf:a aac_adtstoasc`); both attempts crash, so it returns
  `“ffmpeg could not remux the MPEG-TS stream”`.
- **A *present but broken* ffmpeg was handled worse than an *absent* one.** `publish_single`
  (`src/media/remux.rs:83`) only kept the `.ts` when `find_ffmpeg()` returned `None`. When
  ffmpeg exists but fails, it propagated an error instead. Nothing was destroyed — the raw
  track is a separate file and is left untouched, which is the important property — but the
  job reported failure where the documented behaviour is *“a failed mux is recoverable”*. This
  was a real, found-by-verification gap. See roadmap item 6.

  > **Fixed 2026-10-19 (roadmap item 6).** Both failure modes now take the same fallback, and
  > a partial `out` left by a failing `ffmpeg` is removed. See
  > [Item 6 — what shipped](#item-6--what-shipped).

fMP4 is unaffected: it is a plain byte concatenation (`concat`, `src/media/remux.rs:40`) with
no external process, and it is covered by unit tests.

### Claim 3 — “Static analysis only — no disassembler was ever available”

> *No disassembler was ever available (Ghidra missing, IDA unconfigured), so the IDM
> reverse-engineering is shipped-binary static analysis, not dynamic tracing.*

**Verdict: PARTLY CORRECTED. No *decompiler* is available and no *dynamic* observation is
possible — but instruction-level disassembly now works, and the original artifact is still on
disk.**

Deep-analysis providers are indeed still unconfigured (OBSERVED, via REA `binary_session`):

| Provider | Status | Reason |
| --- | --- | --- |
| Ghidra | unavailable | `GHIDRA_INSTALL_DIR` unset |
| Hopper | unavailable | `/opt/hopper/bin/Hopper` missing (and macOS-only) |
| IDA | unavailable | `REA_IDA_MCP_CONFIG` unset |
| rizin | unavailable | `ENOENT` — the `rizin` binary is not installed |

But the claim’s blanket “no disassembler” is now wrong on two counts. First, the target is
still there and still hash-matches:

```console
$ sha256sum "/mnt/c/Program Files (x86)/Internet Download Manager/IDMan.exe"
d1a955acacb65a6ea72ad3cbf06dc35ba48245d04da6659d93f9ea03d165f199  …/IDMan.exe
```

Second, GNU `objdump` is present and understands the PE, so a full disassembly succeeds:

```console
$ objdump -d -M intel IDMan.exe > dis.txt
$ grep -cP '^\s+[0-9a-f]+:' dis.txt
854785
$ objdump -h IDMan.exe        # .text = 0x27acdc bytes at VMA 0x401000, PE32/i386
$ objdump -f IDMan.exe        # start address 0x0062e1fe
```

854,785 instructions is enough to answer questions that `strings` could not. A worked example:
the `.internetdownloadmanager.com/` literal sits at VMA `0x67defc`, and it has **six** code
sites that consume it — `push 0x67defc` at `0x403b95`, `0x403c8b`, `0x47d835`, `0x47fe36`,
`0x52405c`, `0x524172`. Those are the functions that build the browser-integration handshake
address; string-only analysis could show the suffix exists, not who uses it.

**What is still impossible, and why it matters:**

- No **decompilation** / control-flow recovery — the report’s `[INFERRED]` labels on
  algorithmic details (exact retry math, the chunk-splitting heuristic) stay `[INFERRED]`.
- No **dynamic** observation. The target is a Windows PE and this host is Linux; there is no
  Wine and no Windows. Unpacked or not, it cannot be run, traced, or attached to here.

So the honest method statement is now: *disassembly available (objdump), decompilation and
dynamic tracing not*. The report’s own wording should be read with that correction.

### Claim 4 — “No real browser exercised the extension”

> *The extension is covered by 13 Node framing tests plus the daemon protocol tests.*

**Verdict: OBSERVED — confirmed.**

```console
$ grep -riE "playwright|puppeteer|selenium|chromedriver|geckodriver" \
       --include='*.json' --include='*.mjs' --include='*.js' --include='*.rs' --include='*.toml' .
$   # no matches
```

There is no browser-automation dependency anywhere in the tree. Extension coverage is exactly
`extension/tests/protocol.test.mjs` (13 framing tests) plus the daemon’s WebSocket protocol
tests in `tests/daemon.rs`. The `MSG#…` wire format is tested; `background.js`, the popup, the
options page, and the MV3 service-worker lifecycle are **not** exercised against a live
browser, and neither is `manifest.firefox.json` against Firefox.

---

## Part 2 — Platform support, in the same terms

“Cross-platform” is a design property of this codebase. It is **not** yet a verified one.

OBSERVED:

```console
$ rustup target list --installed
x86_64-unknown-linux-gnu
$ ls .github/workflows
ls: cannot access '.github/workflows': No such file or directory
```

- The source is genuinely platform-neutral. The **only** OS-conditional code in the whole
  tree is `write_all_at` (`src/storage.rs:99` `#[cfg(unix)]`, `:117` `#[cfg(windows)]`) — the
  same positioned-write routine expressed against each OS’s `FileExt`. No platform-specific
  crate is pulled in.
- But exactly one target is installed, there is **no CI**, and there is no packaging of any
  kind. macOS and Windows are *intended*, never built and never run. The Windows branch of
  `write_all_at` in particular has never executed.

So the community’s loudest ask — “run on something other than Windows” — is answered by the
architecture, not by evidence. Roadmap item 3 is what turns it into evidence.

---

## Part 3 — Roadmap

Ranked by (value × cheapness), not by size. Positions 1–5 are the items asked for; 6–7 are
what the verification above turned up.

| # | Item | Status | Why | Cost | Notes |
| --- | --- | --- | --- | --- | --- |
| 1 | **Checksum verification** (`--checksum sha256:…`) | ✅ **shipped 2026-10-19** | The cheapest real gap; every neighbouring project lists it and the [survey](COMMUNITY-REQUESTS.md) ranks it second | Small | The temp-then-`rename` path already exists (`storage.rs`), so this is a pre-rename hook: hash the temp file, and on mismatch delete the temp and fail the job. Nothing partially-written ever becomes the output. Support at least `sha256:` and `sha512:`; make it work for `grab`, `project` and the daemon API alike |
| 2 | **Daemon authentication** | ✅ **shipped 2026-10-19** | The daemon bound `127.0.0.1:1001` with **no** token (`src/config.rs:69`), which was the blocker on every NAS / headless / remote-control request in the survey | Small–medium | A bearer token, compared in constant time, required on every route once set; a `--token-file`/env source so it is not in the process arguments. With a token the daemon can then safely bind off-loopback — with the caveat that the transport is plain HTTP, which the daemon now says out loud |
| 3 | **Linux + macOS + Windows support, for real** | planned | The single most-repeated ask; today only `x86_64-unknown-linux-gnu` is built and no CI exists (Part 2) | Medium | A CI matrix (`ubuntu`, `macos`, `windows`) running `cargo test` + clippy + fmt; install the other targets; actually exercise the Windows `write_all_at` branch. Static musl builds for Linux come with item 4 |
| 4 | **Headless packaging** | planned | Static binaries + a container image for drop-in NAS use | Small once 3 exists | Mostly build wiring, no new logic: `x86_64-unknown-linux-musl` release artefacts, a `scratch`/`distroless` image, and the daemon as the entrypoint. The extension is optional in this mode — the HTTP API is the interface |
| 5 | **BitTorrent as a sibling binary** | planned | The most-requested feature in the whole ecosystem, and IDM lacks it too (verified: no BitTorrent protocol strings in `IDMan.exe`) | Large | A **separate** binary (`riptide-bt`) that shares the queue, state store, quota and scheduler with the HTTP engine — explicitly **not** a mode inside it. Different wire protocol, different peer model, different failure modes; folding it into the HTTP engine would put both at risk |
| 6 | **MPEG-TS remux hardening** | ✅ **shipped 2026-10-19** | Found during this verification: a *present but broken* ffmpeg fails the job instead of falling back (§Claim 2) | Small | On any ffmpeg failure in `publish_single`/`mux`, fall back to the same keep-`.ts` copy used when ffmpeg is absent, and say so in the warning. Keep the raw track in every case |
| 7 | **Verify the TS path against a working ffmpeg** | ◐ **partly covered 2026-10-19** | The path has still never run end to end; the available build crashes on MPEG-TS input | Small | Either a working ffmpeg on the test host, or an `#[ignore]`d test gated on `find_ffmpeg()` plus a real TS fixture, so a host with a good ffmpeg covers it and a host without does not silently skip |

### Item 1 — what shipped

`src/checksum.rs` holds `Algorithm` (`md5`, `sha1`, `sha256`, `sha512`) and `Checksum`
(algorithm + lowercase hex digest). `Checksum::parse` accepts `sha256:<hex>` (with `:` or `=`,
case-insensitive, `sha-256` too) and a **bare** hex digest, whose length identifies the
algorithm unambiguously — 32/40/64/128 characters — and which is refused at any other length,
because guessing an algorithm would make a mismatch mean nothing.

Verification happens in `Engine::verify_checksum`, called *before* every rename:

| Path | Where | On mismatch |
| --- | --- | --- |
| HTTP, segmented (`run_segmented`) | `src/engine.rs` — before `sink.finalize` | working `.part` deleted, job fails, output untouched |
| HTTP, single-stream (`run_single`) | `src/engine.rs` — before `sink.finalize` | same |
| FTP (`src/ftp.rs`, both the resume-complete and the normal path) | before `sink.finalize` | same |
| HLS/DASH | `src/media/mod.rs`, on the assembled file | the assembled file is deleted |

The `--checksum` flag is on `download`, `grab --download`, `project add` (`file` kind only, and
`project show` prints it), and the daemon's `POST /api/jobs` JSON (`message_from_json` maps
`checksum` through). It is **refused** — with an explanation, not silently ignored — for
multiple URLs, for `grab --archive`, and for a `site` project: one digest cannot describe many
files.

Two details worth recording, because they are the reason it is a pre-rename hook and not a
post-download check:

- The working file, not the output, is hashed. A failed checksum therefore never touches an
  existing good file — proven by an end-to-end test that plants one and asserts it is verbatim
  afterwards.
- The working file is **deleted** on mismatch. Leaving a known-bad `.part` behind would let a
  later resume finish a corrupt download and call it complete.

Evidence: 9 unit tests in `src/checksum.rs` (published vectors for all four algorithms, the
length inference, every malformed spec, streaming a >64 KiB file, serde round-trip) plus
`tests/checksum.rs` — 6 end-to-end tests against a real HTTP server with range support.

### Item 2 — what shipped

`src/auth/token.rs` holds the whole credential story: `Auth` (the token as a SHA-256 digest) and
`load`, which resolves it from the first source that is *configured*.

| Source | Precedence | Why it exists |
| --- | --- | --- |
| `--token-file PATH` | 1 | Per-run control. A **path**, so the secret never appears in `argv`, which `ps auxww` shows to every user on the machine |
| `RIPTIDE_DAEMON_TOKEN` | 2 | Containers and service managers, which inject a secret without writing a file |
| `daemon_token_file` in `config.json` | 3 | Set once with `riptide config set daemon_token_file /etc/riptide/token`; `config set … none` unsets it |

Three decisions are load-bearing:

- **Constant-time comparison.** Both sides are SHA-256-hashed first, so the compare is over a
  fixed 32 bytes whatever the input length, and neither a correct *prefix* nor a wrong *length*
  is observable in timing. `constant_time_compare_agrees_with_equality` pins the behaviour.
- **A broken source is a startup failure.** A missing file, an empty file, or an empty variable
  aborts with a message instead of falling through to "no token". A typo in a path is the one
  failure mode that must never turn authentication off quietly.
- **Enforcement covers everything.** `handle_http` checks after parsing and before routing, so
  the control page, `/health`, `/api/jobs`, `/api/quota`, `/api/projects` and every future route
  are covered by construction. `handle_ws` checks inside the handshake callback, before the
  subprotocol is negotiated, so an unauthenticated client never gets a socket at all.

Two carriers are accepted, because a browser cannot set a header on a WebSocket handshake:
`Authorization: Bearer <token>`, and `?token=<token>` (percent-decoded). Either one is enough;
they are the same secret, so a request carrying a stale copy of one alongside a valid copy of the
other is not punished for the stale one.

What the token does **not** fix is the transport: the daemon speaks plain HTTP and a plain `ws://`
handshake, so `serve_on` now prints — and logs — a note on startup when it is bound off loopback,
saying that the token travels in the clear and that TLS belongs in front of it. The inverse case
gets a warning instead: off loopback with no token is a hole, and the message says so and names
the two ways to fix it. Both texts come from `exposure_notes`, which is unit-tested rather than
printed into a captured stream.

One gap in the *default* (tokenless, loopback) configuration turned out to be worth closing in
the same change. A page on another origin can POST to `http://127.0.0.1:1001` — the classic
localhost-service problem — and while it cannot read the response, the download would already be
queued. So **a POST must now declare `Content-Type: application/json`** (`is_json_request`, `415`
otherwise). Browsers treat that media type as non-simple, so any cross-origin request carrying it
is preflighted first; the daemon sends no CORS headers and answers no preflight, so the request
never happens. The content types a form or a plain `fetch` *can* send are exactly the ones now
refused. The extension is unaffected: it adds jobs over the WebSocket, not over HTTP.

Evidence: 10 unit tests in `src/auth/token.rs` (exact, wrong, prefix-of and overlong tokens;
scheme casing; the three-source precedence in both directions; every broken-source shape;
freshness and `0600` permissions on a generated file; the constant-time compare), 4 more in
`src/daemon.rs` (`query_param` decoding, the either-carrier rule, `exposure_notes`, and
`is_json_request`'s accept/refuse table). `tests/daemon.rs` adds 7 protocol tests: every route 401
then 200, the 401 body and its `WWW-Authenticate` scheme, both carriers, the refused handshake,
the bearer header on a handshake, a web-page-shaped POST refused with nothing queued, and the
tokenless default unchanged.

### Item 6 — what shipped

`src/media/remux.rs` gained `keep_raw`: one fallback used by **both** failure modes, because
from the user's side they are the same event. `publish_single` and `mux` now fall back when
`ffmpeg` is absent **or** fails, and `keep_raw` removes any partial `out` a failing `ffmpeg`
left behind so a truncated file cannot be mistaken for a finished one. Where the mux previously
returned an error, it now keeps the video track, exactly as it already did when `ffmpeg` was
missing.

Evidence: 4 unit tests, one of which uses **whatever `ffmpeg` the host actually has** (via
`EnvVar::unset("RIPTIDE_FFMPEG")`) and asserts that publishing an MPEG-TS track can never fail
and the raw track always survives — so absent, broken, and working-but-handed-bad-input are all
covered, and item 7's "host with a good ffmpeg" case degrades to this same assertion rather
than silently skipping. The unit tests that mutate `RIPTIDE_FFMPEG`/`PATH` now hold a process
lock, because `cargo test` runs them on threads.

**Explicitly not planned**, for the same reasons they were dropped from IDM’s design
([README](../README.md#how-riptide-differs-from-idm)): kernel filter drivers, redirecting the
browser through a local proxy, silent `declarativeNetRequest` rewrites, a PID-trusted native
messaging host, and any DRM circumvention or per-site signature layer.

---

## Part 4 — Reproducing the checks above

```sh
export PATH="$HOME/.cargo/bin:$PATH"

# Claim 1 — the mock is a mock, FTPS is refused, PAC is never evaluated
sed -n '52,80p' tests/negotiate.rs
sed -n '37,40p' src/ftp.rs
sed -n '61,72p' src/net/client.rs
ls tests/                                   # no media end-to-end test

# Claim 2 — ffmpeg is present, and crashes on MPEG-TS input
command -v ffmpeg && ffmpeg -version | head -1
ffmpeg -y -f lavfi -i "testsrc=duration=2:size=320x240:rate=15" \
       -f lavfi -i "sine=frequency=440:duration=2" -c:v mpeg2video -c:a aac -f mpegts /tmp/t.ts
ffmpeg -y -i /tmp/t.ts -c copy /tmp/t.mp4 ; echo "exit=$?"   # 139 on this host

# Claim 3 — the artifact is still there; disassembly works; decompilers do not
sha256sum "/mnt/c/Program Files (x86)/Internet Download Manager/IDMan.exe"
objdump -d -M intel /tmp/opencode/idm/IDMan.exe | grep -cP '^\s+[0-9a-f]+:'

# Claim 4 — no browser automation exists
grep -riE "playwright|puppeteer|selenium|chromedriver|geckodriver" \
     --include='*.json' --include='*.mjs' --include='*.js' --include='*.rs' .

# Platform support
rustup target list --installed
ls .github/workflows 2>/dev/null || echo "no CI"
```

## Part 5 — Reproducing items 1 and 6

```sh
export PATH="$HOME/.cargo/bin:$PATH"

# Item 1 — the checksum module and its end-to-end guarantee
cargo test --lib checksum
cargo test --test checksum

# A real transfer, checked and unchecked, against a plain HTTP server.
# (python's http.server does not do ranges, so this exercises the single-stream path.)
mkdir -p /tmp/ck && head -c 300000 /dev/urandom > /tmp/ck/data.bin
(cd /tmp/ck && python3 -m http.server 18099 &) ; sleep 1
GOOD=$(sha256sum /tmp/ck/data.bin | cut -d' ' -f1)
riptide download --checksum "sha256:$GOOD" -o /tmp/ck/ok.bin http://127.0.0.1:18099/data.bin
cmp /tmp/ck/ok.bin /tmp/ck/data.bin                     # identical
riptide download --checksum "sha256:$(printf '0%.0s' {1..64})" \
    -o /tmp/ck/bad.bin http://127.0.0.1:18099/data.bin   # fails, publishes nothing
ls /tmp/ck/bad.bin /tmp/ck/bad.bin.part                  # neither exists

# Refused combinations, each with an explanation rather than a silent ignore.
riptide download --checksum "sha256:$GOOD" -o /tmp/ck/ http://127.0.0.1:18099/data.bin \
    http://127.0.0.1:18099/data.bin
riptide grab --archive --checksum "sha256:$GOOD" http://127.0.0.1:18099/

# The daemon API takes the same field; an unparsable one is a 400, a mismatch is a failed job.
riptide daemon --addr 127.0.0.1:18098 &
curl -s -X POST http://127.0.0.1:18098/api/jobs -H 'content-type: application/json' \
     -d "{\"url\":\"http://127.0.0.1:18099/data.bin\",\"checksum\":\"nonsense\"}"   # 400

# Item 6 — every kind of ffmpeg, including the broken one on this host
cargo test --lib media::remux
```

## Part 6 — Reproducing item 2

```sh
export PATH="$HOME/.cargo/bin:$PATH"
# Keep the live check away from the real state and config directories.
export XDG_DATA_HOME=/tmp/rtauth/data XDG_CONFIG_HOME=/tmp/rtauth/config
mkdir -p /tmp/rtauth

# Unit: the token, its sources, its comparison, and the exposure texts.
cargo test --lib auth::token
cargo test --lib daemon

# Protocol: every route, the handshake, and the POST rule.
cargo test --test daemon

# A token file that cannot be read is a startup failure, not an open daemon.
riptide daemon --addr 127.0.0.1:18123 --token-file /tmp/rtauth/absent   # exits non-zero

# Live: generate a token, bind a port, and ask it things as a client would.
riptide daemon --generate-token-file /tmp/rtauth/token
stat -c '%a %n' /tmp/rtauth/token            # 600
TOKEN=$(cat /tmp/rtauth/token)
riptide daemon --addr 127.0.0.1:18123 --token-file /tmp/rtauth/token &
sleep 1

for p in / /health /api/jobs /api/quota /api/projects; do
  curl -s -o /dev/null -w "$p %{http_code}\n" "http://127.0.0.1:18123$p"                 # all 401
  curl -s -o /dev/null -w "$p %{http_code}\n" \
       -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:18123$p"                       # all 200
done

# Either carrier, and a wrong token is not a token.
curl -s -o /dev/null -w '%{http_code}\n' -H "authorization: bearer $TOKEN" http://127.0.0.1:18123/health
curl -s -o /dev/null -w '%{http_code}\n' "http://127.0.0.1:18123/api/quota?token=$TOKEN"
curl -s -o /dev/null -w '%{http_code}\n' -H 'Authorization: Bearer nope' http://127.0.0.1:18123/api/jobs

# A POST a web page could send is refused: a form or plain-fetch content type, or none at all.
curl -s -X POST -H 'content-type: text/plain' -d '{"url":"https://example.com/x"}' \
     http://127.0.0.1:18123/api/jobs                     # 415
curl -s -X POST -d '{"url":"https://example.com/x"}' \
     http://127.0.0.1:18123/api/jobs                     # 415 (curl's form encoding)

# Off loopback the daemon says what the token does and does not protect.
#   warning: … no token is configured …        (no token, off loopback)
#   note: … plain HTTP, so the token travels in the clear …
RIPTIDE_DAEMON_TOKEN=envtok riptide daemon --addr 0.0.0.0:18124 &   # note
```

For the WebSocket half, the relevant property is that the client is a *browser*, which cannot
set a handshake header. Node's built-in `WebSocket` has the same limitation, so it stands in for
one: `GET`-style checks cannot reach `handle_ws` at all, and a `tokio-tungstenite` client happily
sets headers a browser never could. A connection to `ws://127.0.0.1:18123/?cid=x&rnd=1` with no
token is refused (no socket, `handshake rejected`), while `…&token=$TOKEN` completes the
handshake, negotiates `plugin.v3.internetdownloadmanager.com`, and answers `HELLO` with a real
`HELLO_ACK`. That is what was run here:

```sh
node -e '
const url = `ws://127.0.0.1:18123/?cid=ext&rnd=1&token=${process.argv[1]}`;
const ws = new WebSocket(url, "plugin.v3.internetdownloadmanager.com");
ws.onopen = () => ws.send("MSG#1#1#0#0;");
ws.onmessage = (e) => { console.log(e.data); process.exit(0); };
ws.onerror = () => { console.log("refused"); process.exit(0); };
' "$TOKEN"
# without the token: refused
```
