# Riptide — Verification Log and Roadmap

Two things live here:

1. **A verification log.** Four limitations were carried in [`FEATURE-MAP.md`](FEATURE-MAP.md)
   §13 and the README. On 2026-10-19 each was re-checked against the working tree and the
   host. One premise had changed (ffmpeg), one conclusion had changed (a disassembler is now
   available), and two held. The verdicts and the raw evidence are below, so the claims can be
   trusted or re-run rather than taken on faith.
2. **A ranked roadmap** for what to build next, including the requests the
   [community survey](COMMUNITY-REQUESTS.md) surfaced.

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
- **A *present but broken* ffmpeg is handled worse than an *absent* one.** `publish_single`
  (`src/media/remux.rs:83`) only keeps the `.ts` when `find_ffmpeg()` returns `None`. When
  ffmpeg exists but fails, it propagates an error instead. Nothing is destroyed — the raw
  track is a separate file and is left untouched, which is the important property — but the
  job reports failure where the documented behaviour is *“a failed mux is recoverable”*. This
  is a real, found-by-verification gap. See roadmap item 6.

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

| # | Item | Why | Cost | Notes |
| --- | --- | --- | --- | --- |
| 1 | **Checksum verification** (`--checksum sha256:…`) | The cheapest real gap; every neighbouring project lists it and the [survey](COMMUNITY-REQUESTS.md) ranks it second | Small | The temp-then-`rename` path already exists (`storage.rs`), so this is a pre-rename hook: hash the temp file, and on mismatch delete the temp and fail the job. Nothing partially-written ever becomes the output. Support at least `sha256:` and `sha512:`; make it work for `grab`, `project` and the daemon API alike |
| 2 | **Daemon authentication** | The daemon binds `127.0.0.1:1001` and has **no** token (`src/config.rs:69`), which is the blocker on every NAS / headless / remote-control request in the survey | Small–medium | A bearer token in `Config`, compared in constant time, required on every route once set; a `--token-file`/env source so it is not in the process arguments. With a token the daemon can then safely bind off-loopback |
| 3 | **Linux + macOS + Windows support, for real** | The single most-repeated ask; today only `x86_64-unknown-linux-gnu` is built and no CI exists (Part 2) | Medium | A CI matrix (`ubuntu`, `macos`, `windows`) running `cargo test` + clippy + fmt; install the other targets; actually exercise the Windows `write_all_at` branch. Static musl builds for Linux come with item 4 |
| 4 | **Headless packaging** | Static binaries + a container image for drop-in NAS use | Small once 3 exists | Mostly build wiring, no new logic: `x86_64-unknown-linux-musl` release artefacts, a `scratch`/`distroless` image, and the daemon as the entrypoint. The extension is optional in this mode — the HTTP API is the interface |
| 5 | **BitTorrent as a sibling binary** | The most-requested feature in the whole ecosystem, and IDM lacks it too (verified: no BitTorrent protocol strings in `IDMan.exe`) | Large | A **separate** binary (`riptide-bt`) that shares the queue, state store, quota and scheduler with the HTTP engine — explicitly **not** a mode inside it. Different wire protocol, different peer model, different failure modes; folding it into the HTTP engine would put both at risk |
| 6 | **MPEG-TS remux hardening** | Found during this verification: a *present but broken* ffmpeg fails the job instead of falling back (§Claim 2) | Small | On any ffmpeg failure in `publish_single`/`mux`, fall back to the same keep-`.ts` copy used when ffmpeg is absent, and say so in the warning. Keep the raw track in every case |
| 7 | **Verify the TS path against a working ffmpeg** | The path has still never run end to end; the available build crashes on MPEG-TS input | Small | Either a working ffmpeg on the test host, or an `#[ignore]`d test gated on `find_ffmpeg()` plus a real TS fixture, so a host with a good ffmpeg covers it and a host without does not silently skip |

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
