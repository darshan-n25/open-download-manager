# Riptide — Verification Log and Roadmap

Two things live here:

1. **A verification log.** Four limitations were carried in [`FEATURE-MAP.md`](FEATURE-MAP.md)
   §13 and the README. On 2026-10-19 each was re-checked against the working tree and the
   host. One premise had changed (ffmpeg), one conclusion had changed (a disassembler is now
   available), and two held. The verdicts and the raw evidence are below, so the claims can be
   trusted or re-run rather than taken on faith.
2. **A ranked roadmap** for what to build next, including the requests the
   [community survey](COMMUNITY-REQUESTS.md) surfaced.
3. **What has shipped from it.** Items 1 (checksum verification), 2 (daemon authentication), 3
   (Linux + macOS + Windows support) and 6 (MPEG-TS remux hardening) were all built on
   2026-10-19; their entries in the roadmap table carry a status, and a section each describes
   what actually landed and how it is tested. Item 3 is the one where "shipped" needs a qualifier:
   the code and the CI exist, but the CI has not run, because this repository has no remote.

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

“Cross-platform” was a design property of this codebase, not a verified one. **Original
observation, 2026-10-19 (kept: this is what was true before item 3):**

```console
$ rustup target list --installed
x86_64-unknown-linux-gnu
$ ls .github/workflows
ls: cannot access '.github/workflows': No such file or directory
```

- The source was genuinely platform-neutral. The **only** OS-conditional code in the whole
  tree was `write_all_at` (`src/storage.rs:99` `#[cfg(unix)]`, `:117` `#[cfg(windows)]`) — the
  same positioned-write routine expressed against each OS’s `FileExt`. No platform-specific
  crate was pulled in.
- But exactly one target was installed, there was **no CI**, and there was no packaging of any
  kind. macOS and Windows were *intended*, never built and never run. The Windows branch of
  `write_all_at` had never executed.

### Update, later the same day — after item 3

OBSERVED on the authoring machine (x86_64 Linux, no passwordless package install):

```console
$ rustup target list --installed
x86_64-unknown-linux-gnu
x86_64-unknown-linux-musl
aarch64-unknown-linux-gnu
x86_64-apple-darwin
aarch64-apple-darwin
x86_64-pc-windows-msvc
x86_64-pc-windows-gnu
$ ./scripts/check-portability.sh
ok      x86_64-unknown-linux-gnu
ok      x86_64-unknown-linux-musl
ok      aarch64-unknown-linux-gnu
ok      x86_64-apple-darwin
ok      aarch64-apple-darwin
ok      x86_64-pc-windows-msvc
ok      x86_64-pc-windows-gnu

7 checked, 0 skipped, 0 failed
$ ls .github/workflows
ci.yml
```

What is now true, and what is still only *intended*:

| Claim | Status | Evidence |
| --- | --- | --- |
| The Windows-only code compiles for Windows | **OBSERVED** | `scripts/check-portability.sh` compiles `src/platform.rs` for `x86_64-pc-windows-msvc` and `-gnu` with `rustc --emit=metadata`, which needs only the target’s prebuilt `std` — no cross C toolchain. Not vacuous: a deliberately broken `#[cfg(windows)]` function fails the Windows line while the Linux line still passes |
| The layout is right on Windows | **OBSERVED** (on Linux) | `layout_for(spec, get)` takes the environment as an argument, so the Windows answers — `%LOCALAPPDATA%` for state, `%APPDATA%` for config, `%HOMEDRIVE%`+`%HOMEPATH%`, empty variables — are asserted by tests that run on Linux. `spec()` picks `WINDOWS` with `cfg!(windows)`, which is the one line that cannot be tested from here |
| The Windows code **runs** | **UNVERIFIED** | The whole suite is written into `.github/workflows/ci.yml` for `windows-latest`, and has never executed: the repository has no git remote. No Windows machine, no Wine |
| macOS, either architecture | **UNVERIFIED** | Same: `macos-latest` job written, `x86_64-apple-darwin` cross-build written, neither run. Metadata-only compilation for both Apple targets passes |
| A static Linux binary | **OBSERVED for glibc, UNVERIFIED for musl** | `RUSTFLAGS=-C target-feature=+crt-static` + `--target x86_64-unknown-linux-gnu` gives a binary `ldd` calls `statically linked`, and `scripts/check-headless.sh` runs it with nothing else in the filesystem (item 4). The **musl** artefact — the one the image is built from — is still built only in CI, because `musl-gcc` is not installable here |
| A container image | **UNVERIFIED** (`docker build` never ran) | The Dockerfile, the compose file and the release workflow are written and reviewed; the `image` CI job builds, runs and stops the container. Locally, `scripts/check-headless.sh` checks the claims the image makes — one file, no libc, no `/etc`, no CA bundle, SIGTERM honoured — by reproducing them in a one-file rootfs |
| The archive names are Windows-legal | **OBSERVED** | Live: a page linking `/NUL`, `/trailing.` and `/COM1.txt` mirrors to `~4eUL`, `trailing~2e`, `~43OM1.txt` with the markup rewritten to match |

So the community’s loudest ask is now answered by architecture *and* by a check that runs on one
machine, with the second-machine half written down and explicitly unexecuted. That is a smaller
gap than before, and it is stated rather than implied.

---

## Part 3 — Roadmap

Ranked by (value × cheapness), not by size. Positions 1–5 are the items asked for; 6–7 are
what the verification above turned up.

| # | Item | Status | Why | Cost | Notes |
| --- | --- | --- | --- | --- | --- |
| 1 | **Checksum verification** (`--checksum sha256:…`) | ✅ **shipped 2026-10-19** | The cheapest real gap; every neighbouring project lists it and the [survey](COMMUNITY-REQUESTS.md) ranks it second | Small | The temp-then-`rename` path already exists (`storage.rs`), so this is a pre-rename hook: hash the temp file, and on mismatch delete the temp and fail the job. Nothing partially-written ever becomes the output. Support at least `sha256:` and `sha512:`; make it work for `grab`, `project` and the daemon API alike |
| 2 | **Daemon authentication** | ✅ **shipped 2026-10-19** | The daemon bound `127.0.0.1:1001` with **no** token (`src/config.rs:69`), which was the blocker on every NAS / headless / remote-control request in the survey | Small–medium | A bearer token, compared in constant time, required on every route once set; a `--token-file`/env source so it is not in the process arguments. With a token the daemon can then safely bind off-loopback — with the caveat that the transport is plain HTTP, which the daemon now says out loud |
| 3 | **Linux + macOS + Windows support, for real** | ✅ **shipped 2026-10-19**, with the CI unrun | The single most-repeated ask; before this, only `x86_64-unknown-linux-gnu` was built and no CI existed (Part 2) | Medium | Delivered: `src/platform.rs` (layout, positional writes, reserved names), both platforms’ layouts tested from either one, seven-target compile check, `.github/workflows/ci.yml` (native `ubuntu`/`macos`/`windows` matrix + three cross checks + a static musl build proven with `ldd`), `.gitattributes` pinning LF. Unexecuted: everything that needs a second machine — this repo has no remote |
| 4 | **Headless packaging** | ✅ **shipped 2026-10-19** | Static binaries + a container image for drop-in NAS use | Small now that 3 exists | Delivered: SIGTERM handling in the daemon (it had none — `docker stop` killed it mid-write), `deploy/Dockerfile` (Alpine build stage → `scratch`, with `HOME=/data` so state cannot land outside the volume), `deploy/compose.yaml`, `.github/workflows/release.yml` (static musl / aarch64 / macOS ×2 / Windows archives attached to a GitHub Release, plus a multi-arch image pushed to GHCR), an `image` job in CI that builds the image, runs it and stops it, and `scripts/check-headless.sh`, which checks the same claims here by running a static binary inside a **one-file rootfs**. Unexecuted: `docker build` and `docker stop` themselves (no container runtime on this machine) |
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

### Item 3 — what shipped

The item was written as “a CI matrix, the other targets, and actually exercise the Windows
`write_all_at` branch”. The third part is the interesting one, because it is the part that cannot
be done on this machine, so the work became: make as much of it as possible *checkable from one
platform*, and write down plainly which part is not.

**One file holds the platform differences** — `src/platform.rs`:

| Difference | How it is handled |
| --- | --- |
| Where files live | `layout()` resolves `home`, `data` and `config` from a `Spec` per platform. Windows: `%USERPROFILE%` (else `%HOMEDRIVE%`+`%HOMEPATH%`), state in `%LOCALAPPDATA%`, config in `%APPDATA%`. POSIX: `$HOME`, `$XDG_DATA_HOME`/`$XDG_CONFIG_HOME`, else `~/.local/share` and `~/.config`. Before this, `HOME` was the only lookup and the fallback was `.` — on Windows, that put the config, the queue and every part file in whatever directory the program was started from |
| Positional writes | `write_all_at` keeps the retry loop and calls one of two bindings: `FileExt::write_at` (Unix) or `FileExt::seek_write` (Windows). The loop is therefore tested on every platform the tests run on; only the one-line binding differs |
| Names Windows rejects | `is_reserved_windows_name` recognises `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, with or without an extension and with the trailing dots/spaces Windows discards. `util::sanitize_filename` prefixes `_` (a download named `NUL.txt` used to write to the null device); `archive::encode_segment` escapes it as `~xx`, and escapes a *trailing dot* too, so `a.` cannot silently become `a`. Both rules apply on every platform, because a mirror has to be the same mirror wherever it was written |

Also fixed on the way: `sanitize_filename` cut long names with `s[..200]`, which **panics** when
byte 200 lands inside a multi-byte character — reachable from a percent-encoded
`Content-Disposition` filename. It now cuts at the nearest character boundary.

**Checked from one machine.** `scripts/check-portability.sh` compiles `src/platform.rs` for seven
targets with `rustc --emit=metadata`, which resolves `std` for the target but never links — so it
needs no cross C toolchain, no SDK and no second machine:

```console
$ ./scripts/check-portability.sh
ok      x86_64-unknown-linux-gnu
ok      x86_64-unknown-linux-musl
ok      aarch64-unknown-linux-gnu
ok      x86_64-apple-darwin
ok      aarch64-apple-darwin
ok      x86_64-pc-windows-msvc
ok      x86_64-pc-windows-gnu

7 checked, 0 skipped, 0 failed
```

And it is not a rubber stamp. Adding a deliberately broken Windows-only function to that file
fails the Windows line while the Linux line still passes:

```console
$ printf '\n#[cfg(windows)]\nfn windows_only_regression() -> u32 { "not a number" }\n' >> /tmp/broken.rs
$ rustc --edition 2021 --crate-type lib --emit=metadata --target x86_64-pc-windows-msvc \
      --out-dir /tmp/out /tmp/broken.rs
error[E0308]: mismatched types
   --> /tmp/broken.rs:457:39
    |
457 | fn windows_only_regression() -> u32 { "not a number" }
    |                                 ---   ^^^^^^^^^^^^^^ expected `u32`, found `&str`
$ rustc ... --target x86_64-unknown-linux-gnu /tmp/broken.rs    # exit 0: the branch is not compiled here
```

**The whole crate for a foreign target is a different matter**, and it is why CI still matters.
`cargo check --target x86_64-unknown-linux-musl` fails on this host, before reaching a single line
of Riptide, inside `ring`:

```console
$ cargo check --locked --target x86_64-unknown-linux-musl
  cargo:warning=Compiler family detection failed due to error: ToolNotFound: failed to find tool
  "x86_64-linux-musl-gcc": No such file or directory (os error 2)
  error occurred in cc-rs: failed to find tool "x86_64-linux-musl-gcc": No such file or directory
```

That is a real finding with a real consequence for CI: the `cc` crate looks for
`x86_64-linux-musl-gcc`, which Debian's `musl-tools` does **not** provide — it provides
`musl-gcc` — so the musl job sets `CC_x86_64_unknown_linux_musl=musl-gcc` and
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc` explicitly. Installing `musl-tools` and
hoping is not enough.

**CI.** `.github/workflows/ci.yml` has five jobs: `test` (the full suite on `ubuntu-latest`,
`macos-latest`, `windows-latest`, with clippy `-D warnings` and `rustfmt --check`, the extension’s
`node --test`, and an `x86_64-apple-darwin` cross-build from the arm64 macOS runner), `cross`
(`cargo check --all-targets` for musl, aarch64-linux-gnu and windows-gnu, each with `CC_<target>`
and a linker set), `static` (a release musl binary, `file`d, `ldd`ed to prove it is static, run
once, uploaded), `image` (item 4: build the Dockerfile, run the container, probe `/health`, stop
it with SIGTERM and assert the exit code is 0, then run `scripts/check-headless.sh`), and
`portability` (the script above). `.gitattributes` pins checkouts to LF so a Windows clone is
byte-identical.

The honest caveat, repeated because it is the whole point: **that workflow has never run.** This
repository has no remote. Everything above that is tagged OBSERVED was run here; the workflow is
the part that needs a second machine, and it is unexecuted rather than "tested".

Evidence for the parts that did run: 15 tests in `src/platform.rs` (both platforms’ layouts,
the split `%HOMEDRIVE%`, an empty variable, no home at all; the write loop against a
three-bytes-a-time writer, a zero-length write, an over-long write and a real error; the device
name table), 2 new in `src/util.rs` (device names, and a 300-character multi-byte name cut
without splitting a character), 1 new in `src/archive.rs` (`...` → `..~2e`, `NUL` → `~4eUL`,
`nul.txt` → `~6eul.txt`, `COM10` unchanged, and that the escapes stay injective), and a live
archive of a page whose links are `/NUL`, `/trailing.` and `/COM1.txt`.

### Item 4 — what shipped

The item was “static binaries + a container image for drop-in NAS use”. Reading it properly
turned up something the wording hid: **the daemon had no shutdown path at all.** `cmd_daemon`
called `serve_on` and never returned; `spawn_ctrl_c` — the only signal handling in the program —
was wired into `download`, `grab` and `queue`. So Ctrl-C in a terminal killed the daemon the way
the Unix default does, and `docker stop` (SIGTERM), `systemctl stop` and `kubectl delete` killed
it **mid-write** with no exit code. A container is the environment where that matters most, which
is presumably why it survived: nothing here had ever been run as a service.

**The stop is now a real path.** Three pieces:

- `Scheduler` counts what is running (an `AtomicUsize` raised by an RAII guard in
  `execute_with`, and by a due-project sweep) and exposes `wait_for_idle(grace)`. The guard
  matters: every exit from a job, including the `?` on a failed save, has to decrement, or a stop
  would wait for work that already ended.
- `daemon::bind` now returns the stop flag as well as the listener and the scheduler, because
  only the caller knows when to raise it — and a `Scheduler` with no flag has nothing to watch.
- `cmd_daemon` selects on `serve_on` against `wait_for_stop()`, which is Ctrl-C anywhere and a
  SIGTERM too on Unix (`tokio::signal::unix`). It raises the flag, waits up to five seconds for
  running jobs to record themselves as `paused` and flush the quota, names anything still going,
  and exits 0.

Five seconds is the bound on purpose: a transfer wedged inside a single chunk must not hold
`docker stop` open until Docker escalates to SIGKILL, which would destroy exactly what the wait
was protecting. `stop_grace_period` must exceed it; the compose file uses 15.

**The image.** `deploy/Dockerfile` is a two-stage build whose runtime stage is `scratch`, which
is possible because there is nothing to put in it: TLS is `rustls` with `webpki-roots`, so the
trust roots are compiled in and there is no CA bundle to ship; the binary is musl-static, so
there is no libc; the daemon is PID 1 and reaps its own children, so there is no init shim. The
build stage is Alpine, whose default toolchain *is* musl, so the artefact the runtime stage needs
is the one the build produces — no cross toolchain, no `musl-gcc` detection problem (the one item
3 hit).

`HOME=/data` in the image is not decoration. The layout falls back to the working directory when
there is no home at all, and a runtime that invents `HOME=/root` would put the queue outside the
volume — a queue that cannot resume is the one thing a download manager must never have. One
volume at `/data` then holds the queue, the quota window, the projects, the mirrors, the config
and the downloads.

`deploy/compose.yaml` publishes on the host's loopback by default, so it works out of the box
with no token; reaching it from the network means changing the publish rule **and** setting
`RIPTIDE_DAEMON_TOKEN` in the same edit, which the file says in the place you would be editing.

**The release.** `.github/workflows/release.yml` builds on a `v*` tag: static musl and
aarch64-glibc Linux, both macOS architectures, Windows MSVC; proves each Linux archive is static
with `ldd` before it is published; attaches the archives to a GitHub Release with generated
notes; and builds a multi-arch image to `ghcr.io`. The `image` job in `ci.yml` builds the
Dockerfile on every commit, runs the container, probes `/health`, and stops it with `docker stop`
— then asserts the container's exit code is 0, which is the one thing a `scratch` image cannot be
inspected for after the fact.

**What was checked, and how, on a machine with no container runtime.** `scripts/check-headless.sh`
builds a fully static binary (`crt-static`, glibc) and runs it inside a rootfs that contains
exactly what the image contains — one file — using an unprivileged user namespace for the chroot:

```console
$ RUSTFLAGS='-C target-feature=+crt-static' cargo build --release --locked --target x86_64-unknown-linux-gnu
$ ldd target/x86_64-unknown-linux-gnu/release/riptide
	statically linked
$ ./scripts/check-headless.sh
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

That is a *glibc* static binary rather than the musl one the image is built from. The claim is
the same one — no interpreter, no shared libraries — but the artefact is not, and the musl one is
still only built in CI. It is written here rather than left implied.

The same signal claims are asserted against the released binary with no container, no chroot and
no namespace, in `tests/shutdown.rs`: three tests start `riptide daemon`, start a trickling
origin, send a **real SIGTERM** (and SIGINT, as a control), and check that the process exits 0
with a line naming the signal, that the interrupted job is on disk as `paused` with some bytes
but not all of them, and that an idle daemon stops at once rather than sitting out the grace
period. A stop that was merely *fast* would fail the pause assertion; a stop that never drained
would fail the exit-code one.

Unit-level, `src/scheduler.rs` gained two: an idle scheduler reports idle immediately, and a
scheduler with a unit of work registers a waiter that is woken by the work ending rather than by
the deadline — with the notification raised in a way that would be missed if the waiter were
registered after the count was read.

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
ls .github/workflows                    # ci.yml — written, and unrun (no remote)
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

---

## Part 7 — Reproducing item 3

Everything below was run on x86_64 Linux with no cross C toolchain, no SDK and no second
machine — which is the point: it is the part of “Windows support” that one box can check.

```sh
export PATH="$HOME/.cargo/bin:$PATH"

# Seven targets, then the check that needs no toolchain for any of them.
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-gnu \
                 x86_64-apple-darwin aarch64-apple-darwin \
                 x86_64-pc-windows-msvc x86_64-pc-windows-gnu
./scripts/check-portability.sh
#   ok      x86_64-unknown-linux-gnu
#   ok      x86_64-unknown-linux-musl
#   ok      aarch64-unknown-linux-gnu
#   ok      x86_64-apple-darwin
#   ok      aarch64-apple-darwin
#   ok      x86_64-pc-windows-msvc
#   ok      x86_64-pc-windows-gnu
#   7 checked, 0 skipped, 0 failed

# It has teeth: a Windows-only mistake fails the Windows line and leaves Linux alone.
d=$(mktemp -d); cp src/platform.rs "$d/broken.rs"
printf '\n#[cfg(windows)]\nfn windows_only_regression() -> u32 { "not a number" }\n' >> "$d/broken.rs"
rustc --edition 2021 --crate-type lib --emit=metadata --target x86_64-pc-windows-msvc \
      --out-dir "$d" "$d/broken.rs"        # error[E0308]: mismatched types
rustc --edition 2021 --crate-type lib --emit=metadata --target x86_64-unknown-linux-gnu \
      --out-dir "$d" "$d/broken.rs"        # exit 0 — the branch is not compiled here
rm -rf "$d"

# The layout is a pure function of the environment, so both platforms' answers are asserted
# from either one. 15 + 9 + 24 tests.
cargo test platform::        # 15
cargo test util::            # 9
cargo test archive::         # 24

# Live: what this Linux box resolves, in a directory of its own.
mkdir -p /tmp/rtplat/{home,data,config}
export HOME=/tmp/rtplat/home XDG_DATA_HOME=/tmp/rtplat/data XDG_CONFIG_HOME=/tmp/rtplat/config
riptide config path                          # /tmp/rtplat/config/riptide/config.json
riptide config show | grep -E 'download_dir|state_dir'
#   "download_dir": "/tmp/rtplat/home/Downloads"
#   "state_dir": "/tmp/rtplat/data/riptide"

# The last resort is unchanged, and is still the working directory.
env -u HOME -u XDG_DATA_HOME -u XDG_CONFIG_HOME riptide config path
#   ./.config/riptide/config.json

# Live: a URL that is a device name, or that ends in a dot, becomes a name Windows can store.
mkdir -p /tmp/rtnul/site && cd /tmp/rtnul/site
printf '<a href="/NUL">a</a><a href="/trailing.">b</a><a href="/COM1.txt">c</a>' > index.html
for f in NUL 'trailing.' COM1.txt; do echo "$f page" > "$f"; done
python3 -m http.server 18132 --bind 127.0.0.1 &
sleep 1
riptide grab --archive -o /tmp/rtnul/mirror http://127.0.0.1:18132/index.html
find /tmp/rtnul/mirror -mindepth 1 | sort
#   /tmp/rtnul/mirror/~43OM1.txt
#   /tmp/rtnul/mirror/~4eUL
#   /tmp/rtnul/mirror/index.html
#   /tmp/rtnul/mirror/_riptide
#   /tmp/rtnul/mirror/_riptide/validators.json
#   /tmp/rtnul/mirror/trailing~2e
cat /tmp/rtnul/mirror/index.html
#   <a href="~4eUL">a</a><a href="trailing~2e">b</a><a href="~43OM1.txt">c</a>
kill %1

# What one machine cannot do, and the exact reason. `ring` is C and assembly, so the whole
# crate for a foreign target needs a cross C compiler that is not installable here.
cargo check --locked --target x86_64-unknown-linux-musl
#   cargo:warning=Compiler family detection failed due to error: ToolNotFound: failed to find
#   tool "x86_64-linux-musl-gcc": No such file or directory (os error 2)
#   error occurred in cc-rs: failed to find tool "x86_64-linux-musl-gcc"
```

Note the name in that last error: the `cc` crate asks for `x86_64-linux-musl-gcc`, while
Debian's `musl-tools` installs `musl-gcc`. That is why the CI job sets
`CC_x86_64_unknown_linux_musl` explicitly, and it is the kind of thing that would have looked
like a flaky runner instead of a wrong assumption.

What remains unexecuted is exactly what needs a Windows machine, a macOS machine, or a package
manager with sudo: the `test` job's `windows-latest` and `macos-latest` legs, the three `cross`
targets, and the static musl build. All four are written; none has run.

---

## Part 8 — Reproducing item 4

Everything here was run on x86_64 Linux with **no container runtime of any kind** — no `docker`,
`podman`, `buildah` or `containerd` — which is the reason this part is shaped the way it is: the
image is checked by reproducing what it *is*, rather than by building it. `docker build` and
`docker stop` appear in `ci.yml` and nowhere else, and they have never run.

```sh
export PATH="$HOME/.cargo/bin:$PATH"

# A fully static binary. `crt-static` on the glibc target is the substitute here for the musl
# build the Dockerfile produces, because `musl-gcc` is not installable on this machine.
RUSTFLAGS='-C target-feature=+crt-static' cargo build --release --locked --target x86_64-unknown-linux-gnu
ldd target/x86_64-unknown-linux-gnu/release/riptide
#   statically linked

# The image's claims, on a machine with nothing to build an image with. The rootfs it builds
# holds one file; the chroot is an unprivileged user namespace.
./scripts/check-headless.sh
#   == 1. a static binary == 11984160 bytes, ldd says: statically linked
#   == 2. an image that is one file == /image/usr/local/bin/riptide, /image/data
#   == 3. where it keeps its files == no HOME -> ./.config/riptide/config.json
#                                     HOME=/data -> /data/.config/riptide/config.json
#   == 4. a real download == 6 MiB over 4 connections, byte-identical
#   == 5. the daemon serves the API == /health 200, / 200, POST /api/jobs 201
#   == 6. docker stop, mid-download == exit=0 after 1s, job paused at 2070976 of 12582912
#   all checks passed

# The same signal claims against the released binary, with no container, chroot or namespace
# involved: a real SIGTERM into a real process with a real transfer in flight.
cargo test --test shutdown
#   sigterm_stops_the_daemon_cleanly_mid_download ... ok
#   sigint_still_stops_the_daemon ... ok
#   a_stop_with_an_idle_queue_is_immediate ... ok

# The two halves of the drain mechanism, at unit level.
cargo test scheduler::
#   an_idle_scheduler_is_idle_at_once ... ok
#   waiting_for_idle_wakes_when_the_work_ends_and_gives_up_at_the_deadline ... ok

# By hand, without any test harness: SIGTERM stops the daemon and it exits 0.
python3 tests/origin.py 18092 /tmp/rtorigin &
mkdir -p /tmp/rtorigin && head -c 6M /dev/urandom > /tmp/rtorigin/payload.bin
export RIPTIDE_CONFIG=/tmp/rtcontainer/config.json HOME=/tmp/rtcontainer
riptide config init
riptide daemon --addr 127.0.0.1:18091 &      # prints the exposure note too, on 0.0.0.0
curl -s http://127.0.0.1:18091/health        # ok
kill -TERM %2; wait %2; echo $?              # SIGTERM … stopped
#   0                                        ← not 143: the process stopped itself
riptide list                                 # the queue is intact
```

Two things are still only written down rather than run: the **musl** binary (built in CI, not
here) and the **image itself** (`docker build`/`docker stop`, in CI, not here). Part 2's table
carries both as UNVERIFIED for exactly this reason.
