# Internet Download Manager — Reverse-Engineering Report

**Purpose:** establish, from the shipped artifact, how IDM works well enough to build a
functionally equivalent open-source download manager.
**Companion document:** [`FEATURE-MAP.md`](FEATURE-MAP.md) maps every observed feature to its
implementation in this repository.

---

## 0. Scope, method, and honest limits

| | |
|---|---|
| **Target** | `IDMan.exe` + its 75 companion files |
| **Location** | `/mnt/c/Program Files (x86)/Internet Download Manager/` (Windows C: via WSL) |
| **SHA-256** | `d1a955acacb65a6ea72ad3cbf06dc35ba48245d04da6659d93f9ea03d165f199` |
| **Size** | 6,057,328 bytes |
| **Format** | PE32 for i386 (`pei-i386`), magic `0x10b` |
| **Reported version** | "Internet Download Manager 6.42" (resource strings); copyright "1999–2026 Tonec FZE" |
| **Build hint** | `X:\Projects\IDMRelease\6.38.20\IDMMsgHost\Release_Win32\IDMMsgHost.pdb` |

**What I did:** PE header/section/import parsing, resource and string extraction (ASCII +
UTF-16LE), companion-binary inspection, archive extraction of the shipped browser extensions
(CRX/XPI), and registry/config-file analysis.

**What I did NOT do — and why it matters:**

- **No disassembly or decompilation.** No working disassembler provider was configured
  (Ghidra `GHIDRA_INSTALL_DIR` unset, Hopper is macOS-only, IDA unconfigured). Everything
  below is derived from *static metadata, symbols, RTTI names, string literals, and the
  shipped extension's JavaScript* — not from control-flow analysis.
- **No runtime observation.** The target is a Windows binary and this host is Linux; I did
  not execute it, sniff its traffic, or attach a debugger.
- Consequently: **data structures and protocol constants are observed**; **algorithmic
  details (exact retry math, exact chunk-splitting heuristic) are inferred** and labelled as
  such. Every claim below is tagged `[OBSERVED]`, `[INFERRED]`, or `[UNKNOWN]`.

**Good news:** `IDMan.exe` is **not packed or obfuscated** in the traditional sense — normal
section layout, `HAS_DEBUG`/`HAS_LOCALS` flags set, and MSVC C++ RTTI class names present in
plaintext. Only the *browser-extension JavaScript* is minified, and only *site names* in it
are lightly ciphered (a ROT-13 variant). So the artifact yields its architecture readily even
without a disassembler.

---

## 1. High-level architecture

IDM is **not one program**. It is a suite of ~76 components cooperating through a shared
config store (registry + `.dat` files), a local socket, and Windows kernel drivers.

```
┌──────────────────────────── browsers ────────────────────────────┐
│  Chrome/Edge/Brave/Cốc Cốc   Firefox   Opera   (IE legacy)       │
│     IDMGCExt.crx            idmmzcc3.xpi  IDMOpExt.nex           │
│         └── "IDM Integration Module" (MV3 service worker) ──┐    │
└─────────────────────────────────────────────────────────────┼────┘
                                                              │
                    ① WebSocket  ws://127.0.0.1:1001          │
                       subprotocol "plugin.v3.internetdownloadmanager.com"
                    ② nativeMessaging "com.tonec.idm"          │
                       └─► IDMMsgHost.exe ──named pipe──► IDM  │
                                                              ▼
┌──────────────────────────── IDMan.exe ───────────────────────────┐
│ UI (MFC/Win32)   Download engine (ChLst)   Scheduler   Grabber   │
│   ├── HTTP/HTTPS client  (raw Winsock, select() loop)            │
│   ├── FTP client         (control + PASV data sockets)           │
│   ├── TLS                (schannel.dll + bundled OpenSSL)         │
│   ├── Auth               (Basic/Digest/Bearer/SSPI)              │
│   ├── HLS parser         (CM3UParser)                            │
│   ├── DASH/MP4 reader    (CMp4DashReader)                        │
│   └── file-type DB       (IDMFType.dat)                          │
└───────┬───────────────────┬──────────────────┬───────────────────┘
        │                   │                  │
   idmwfp64.sys        idmtdi64.sys       IDMNetMon64.dll
   (WFP driver)        (TDI filter)       (traffic monitor)
        │                   │                  │
        └───────── network stack interception ─┘
```

### Companion component census `[OBSERVED]`

| Component | Role (from name + strings) |
|---|---|
| `IDMan.exe` | Main application: UI, engine, scheduler, grabber |
| `IDMGrHlp.exe` | Grabber helper (separate process) |
| `IDMMsgHost.exe` | Browser native-messaging host; relays stdio ↔ named pipe |
| `idmBroker.exe`, `MediumILStart.exe` | Privilege/UAC brokers |
| `IEMonitor.exe` | Legacy IE integration monitor |
| `IDMNetMon.dll` / `IDMNetMon64.dll` | Network traffic monitor |
| `idmwfp32/64/AA.sys` + `.cat`/`.inf` | **Windows Filtering Platform** driver |
| `idmtdi32/64.sys` + `.cat`/`.inf` | Legacy **TDI filter** driver |
| `idmfsa.dll` | File-system agent |
| `idmvconv.dll` | Video converter |
| `IDMVMPrs.dll` / `IDMVMPrs64.dll` | Video/MP4 stream parser |
| `IDMVMPrs`, `IDMGetAll*.dll` | "Get all links from page" engine |
| `IDMIECC*.dll` | IE cache control/COM |
| `IDMShellExt*.dll` | Explorer shell extension |
| `idmbrbtn.dll` | Browser button integration |
| `idmcchandler2*.dll`, `idmcchandler7*.dll` | Content-type / protocol COM handlers |
| `downlWithIDM*.dll` | "Download with IDM" shell verb |
| `IDMFType.dll`/`.dat` | File-type detection database |
| `idmindex.dll`, `idmmkb.dll`, `idmnmcl.dll`, `idmvs.dll` | Indexing, hotkeys, Mozilla client, video service |
| `libssl.dll` / `libcrypto.dll` | Bundled OpenSSL (2020 builds) |
| `IDMIntegrator64.exe` | Installs extensions into browsers |
| `idmantypeinfo.tlb` | COM type library (automation surface) |
| `*.chm` | Help: `idman`, `grabber`, `scheduler`, `tutor` |
| `defexclist.txt` | Shipped default *exclude* URL list for the grabber |
| `tips.txt`, `license.txt` | User-facing text |

---

## 2. The download engine — how the speedup actually works

### 2.1 Only seven statically imported DLLs `[OBSERVED]`

```
KERNEL32.dll  USER32.dll  GDI32.dll  COMDLG32.dll
ADVAPI32.dll  COMCTL32.dll  SHLWAPI.dll
```

No `WS2_32`, no `WININET`, no `WINHTTP`, no `CRYPT32`, no `SHELL32`. Everything else is
loaded at runtime — `[OBSERVED]` via literal strings present in the image:

- `ws2_32.dll` — Winsock
- `wininet.dll` — **only** for proxy autodetection
- `schannel.dll` — TLS
- `secur32.dll` — SSPI (NTLM/Kerberos)
- `ole32.dll`, `OLEAUT32.dll`, `shell32.dll`, `oleacc.dll`, `oledlg.dll`, `ntdll.dll`
- `sporder.dll` — per-process DNS provider ordering
- its own: `idmbrbtn.dll`, `idmftype.dll`, `idmmzcc.dll`, `idmvmprs.dll`, `idmvs.dll`,
  `idmnmcl.dll`, `idmindex.dll`, `idmmkb.dll`, `idmiecc64.dll`, `idmgetall64.dll`,
  `idmshellext.dll`, `idmcchandler2*.dll`, `downlWithIDM64.dll`, `libcrypto.dll`, `libssl.dll`

**Consequence:** IDM ships its **own HTTP/FTP stack**. `WININET` is used purely for
proxy discovery (`WinHttpGetIEProxyConfigForCurrentUser`, `WinHttpDetectAutoProxyConfigUrl`,
`WinHttpOpen`, `WinHttpGetProxyForUrl`). This is the single most important architectural
fact: the download engine is custom, so its behaviour is fully under Tonec's control.

### 2.2 Custom `select()`-based socket engine `[OBSERVED]`

```
GetAddrInfoW
Bind socket OK, try call listen()
Error during call to listen() data socket
Error during call to accept() internet socket.
Error during call to listen() data socket
Select error code = %d, socket = %d
Checking connect, socket = %d
Internet socket read error / write error
File write error occurred while receiving data from Internet socket.
```

→ raw Winsock + `select()` readiness loop + hand-rolled connect/accept/timeout handling.

### 2.3 Segmented byte-range transfer `[OBSERVED]`

Format strings found verbatim in `.rdata`:

```
Range: bytes=0-
Range: bytes=0-%I64d
Range: bytes=%I64d-
Range: bytes=%I64d-%I64d
Content-Range:
Content-Length:
Transfer-Encoding:
Connection: Keep-Alive
```

Supporting protocol vocabulary:

```
Accept-Ranges (implied by 206 handling)
multipart/byteranges
multipart/x-byteranges      ← server returned several ranges in one body
ETag:
If-Range:                   ← makes resume safe against a changed file
Last-Modified:
```

- `%I64d` is the MSVC 64-bit printf specifier ⇒ segment offsets are **64-bit**, so files
  larger than 4 GB are supported.
- `If-Range:` + `ETag:`/`Last-Modified:` ⇒ IDM **conditionally revalidates** before resuming.
  If the resource changed, the validator fails and IDM must restart rather than splice stale
  bytes. This is the correct, spec-sanctioned resume strategy.
- `multipart/byteranges` handling ⇒ IDM can accept a server that collapses multiple requested
  ranges into one multipart response instead of per-range responses.

### 2.4 The chunk-list engine and *dynamic* chunk sizing `[OBSERVED]`

MSVC RTTI/error strings expose the central data structure — `ChLst` (**Chunk List**):

```
ChunksList::CreateNewItem()
Memory allocation error in ChunksList::CreateNewItem()
Insert next chunk in list
Erase chunk_list.
Erase chunk_list f2.
Change chunk size to %ld
Chunk order violation error, cInf.ID %ld, startPos %I64d
ChLst err 2586 / 1515 / 2577
Unkerr in ChLst::OnSDID 1515 / 2577
next chunk was completed too.
Disk is full. Cannot add next chunks.
Not enough disk space to store files chunks, canceling download.
Not enough disk space to rebuild downloaded chunks.
Cannot rebuild downloaded file chunks.
New connection to the same server was added.
```

Reading this together:

1. The file is modelled as a mutable **list of chunks** (`ChLst`), not a fixed set of N
   segments. Chunks are created, inserted, and erased at runtime.
2. `"Change chunk size to %ld"` + `Chunk order violation error` ⇒ IDM **dynamically resizes
   chunks mid-download**. `[INFERRED]` This is IDM's real "dynamic segmentation": rather than
   splitting into a fixed number of equal parts up front, it starts chunks, measures each
   connection's throughput, and re-splits the *remaining* unfinished region so that slow
   servers still get more meaningful parallelism, while finished regions are erased.
3. `"...rebuild downloaded file chunks"` ⇒ IDM assembles the final file from chunk data, and
   the chunk data is materialised on disk (hence the disk-space checks). `[INFERRED]` Chunks
   are written into their final offsets or staged and then compacted; the disk-space checks
   before *adding* a chunk strongly suggest **preallocation**, since IDM refuses to start a
   chunk it cannot fit.
4. `New connection to the same server was added.` ⇒ worker count is dynamic, bounded by a
   per-server limit.

### 2.5 Connection lifecycle `[OBSERVED]`

```
Connection %d, downloaded %I64d.
Connection failed.
Connection failed, restarting...
Connection restarted because of timeout
Connection restarted because of timeout (1)
Connection restarted because of timeout (2)
Connection has been closed by server.
Connection has been closed by the server.
Connection is restarted...
Connection timed out
Connection timed out, restarting...
```

⇒ each worker connection has independent timeout detection, retry, and restart. There are at
least two distinct timeout classes `(1)` and `(2)` — `[INFERRED]` an idle/read timeout and a
connect timeout.

### 2.6 Tunables exposed in the UI `[OBSERVED]`

```
MaxConnectionsNumber                       (config key)
"Max. connections number"                  (UI label)
"Max. connections number for a server"     (UI label — per-host cap)
"Maximum download speed:"                  (global)
"Maximum download speed for one file"      (per-job cap)
"Number of retries for each file if downloading failed :"
"Connections and Limits"                   (options page)
```

IDM therefore has **global** concurrency, **per-server** concurrency, **global** bandwidth
cap, **per-file** bandwidth cap, and a retry budget — all independent knobs.

### 2.7 Multipart and chunked transfers `[OBSERVED]`

`chunked`, `Transfer-Encoding:`, `Content-Length:` plus the `multipart/*` handling mean the
engine must cope with responses that are *not* seekable. `[INFERRED]` When `Accept-Ranges` is
absent or the response is chunked/compressed, IDM falls back to a single streaming connection.

---

## 3. Authentication and credentials `[OBSERVED]`

Literal response headers built by IDM:

```
Authorization: Basic %s
Authorization: Digest username="%s", realm="%s", nonce="%s", uri="%s", %sresponse="%s"%s
Authorization: Bearer / BearerAuth
Proxy-Authorization: Basic %s
Proxy-Authorization: Digest username="%s", realm="%s", nonce="%s", uri="%s", %sresponse="%s"%s%s
Cookie: / Cookie2: / Referer:
```

Notes:

- **Digest** is implemented directly, including the `%s` slot before `response=` — that slot
  carries optional attributes such as `qop=auth, nc=..., cnonce=...`. `[INFERRED]`
- **Bearer** tokens are supported (likely for sites that hand out OAuth tokens).
- **SSPI / Windows-integrated auth** is via `secur32.dll`:
  `AcquireCredentialsHandleA`, `DecryptMessage`, and errors logged as
  `"**** Error 0x%x returned by AcquireCredentialsHandle"`. `AcquireCredentialsHandle` is the
  SSPI entry point for **Negotiate (SPNEGO → Kerberos/NTLM)**. `[INFERRED]` IDM offers
  "Integrated authentication" as an auth type; the Wikipedia claim of NTLM/Kerberos support is
  consistent with this, even though the strings `NTLM`/`Kerberos` are absent — SSPI hides them
  behind package negotiation.
- A dedicated class `CWebAuthorization` handles the state machine.
- Credentials are stored per-site: `"Options/Sites passwords"`, config key
  `useAuthorization=%d`.
- Classic error surfaces: `Authorization Required`, `Proxy Authorization Required`,
  `"Cannot connect via socks server, authorization failed!"`.
- TLS renegotiation is handled: `"Server requested renegotiate!"`; `BCryptDecrypt` and
  OpenSSL (`RSA_public_decrypt`) both appear, so crypto is mixed: SChannel for the transport,
  CNG/OpenSSL for payload-level work.

---

## 4. Browser integration — the two-channel design `[OBSERVED]`

This is the most precisely recoverable part, because the extension source ships in the clear
(minified but readable). From `IDMGCExt/background.js`:

### 4.1 Channel ① — local WebSocket (primary)

```js
new WebSocket(`ws://127.0.0.1:1001/?cid=${clientId}&rnd=${rand}`,
              "plugin.v3.internetdownloadmanager.com")
```
with a fallback host list `["127.0.0.1:1001", "0.1.0.1:1001"]`, a 15 s keepalive, and
reconnect backoff that doubles with attempt count (capped at ~120 attempts, backing off to
`5s * attempts`). On the wire it is a **custom text protocol, not JSON**.

### 4.2 Channel ② — WebExtension native messaging (fallback)

```js
browser.runtime.connectNative("com.tonec.idm")
```
Manifest `IDMMsgHost.json` `[OBSERVED]`:

```json
{ "name": "com.tonec.idm",
  "description": "Internet Download Manager extension helper",
  "type": "stdio",
  "allowed_origins": [ "chrome-extension://ngpampappnmepgilojfohadhhmbhlaek/",
                       "chrome-extension://llbjbkhnmlidjebalopleeepgdfgcpec/" ],
  "path": "IDMMsgHost.exe" }
```
Firefox variant `IDMMsgHostMoz.json` uses `allowed_extensions:
["mozilla_cc3@internetdownloadmanager.com"]`.

`IDMMsgHost.exe` imports `GetNamedPipeClientProcessId` / `GetNamedPipeServerProcessId` and
contains `"Cannot open pipe"` ⇒ the host is a **stdio ⇄ named-pipe bridge** to the already
running IDM process. Note it also *authenticates the caller by PID* — a (weak) trust check.

### 4.3 The "MSG#" wire format `[OBSERVED]`

The extension's `pb` serializer/parser implements:

```
MSG#<msgId>#<type>#<fieldA>#<fieldB>[,<key>=<value>...][;]
```

- Records are newline-independent; the parser scans for the delimiters `: , ;` with a
  resumable state machine (`readAsBinaryString` in 50/100/25/21-byte slices), so a single
  WebSocket frame or native-message can carry *multiple* records and a record can be split
  across frames.
- Scalars: `string` values are escaped/encoded; `number` and `boolean` are literal;
  `ArrayBuffer`/`Uint8Array`/`Blob` payloads are **base64** with a length prefix —
  `len:data` for inline text, `len%data` for base64.
- `IDMMsgHost.exe` contains the literal `MSG#0#17#1#0:9:` — a real framed sample, matching
  the grammar exactly.

### 4.4 Handshake and versioning `[OBSERVED]`

```js
S(this, 2, 5, 0, [112, 91, Xa?1028:1031, 0, 16845059, clientId, 15, manifestVersion], {...})
```
- `16845059 == 0x01010903` — a packed version constant. (Read the decimal literally; an
  earlier draft of this report mis-transcribed it as `0x01010D03`, which is `16846083`. The
  shipped extension's own handshake is `MSG#…,version=16845059,…`.)
- The extension also parses its own version string `v<major>.<minor>b<build>` into a 32-bit
  integer and gates feature use on thresholds (e.g. `ua < 103430656`, `ua < 103431424`),
  negotiating capabilities with the backend. `[INFERRED]` this is how old IDM builds stay
  compatible with new extensions.

### 4.5 URL hand-off schemes `[OBSERVED]`

Registered custom schemes (function `T`, map `wa`):

| Scheme | id | Purpose |
|---|---|---|
| `http` | 1 | plain HTTP |
| `https` | 9 | TLS |
| `ftp` | 2 | FTP |
| `ftps` | 10 | FTP over TLS |
| `idmreg` | 12 | hand-off "register this download" |
| `idmdwnlmfv9` | 33 | "IDM download" trigger used by the Firefox bridge |

Plus HLS-specific hand-offs seen in `.rdata`:

```
hls2toidm://      hls2toidmtomp4://
hlstoidm://       idmhlstomp4://      idmhlstomp4v2://
```

The extension encodes URLs as `idmdwnlmfv9://<url>` when handing off, strips the URL
fragment, and registers Windows URL-protocol handlers under
`Software\Classes\PROTOCOLS\Name`.

### 4.6 What the extension actually does to the browser `[OBSERVED]`

Extension `manifest.json` (v6.42.64, Chrome MV3, `minimum_chrome_version: 109`):

```
permissions: scripting, tabs, cookies, contextMenus, webNavigation, webRequest,
             declarativeNetRequest, downloads, downloads.shelf, downloads.ui,
             management, storage, proxy, nativeMessaging
host_permissions: <all_urls>
externally_connectable: *://*.internetdownloadmanager.com/*, *://*.tonec.com/*
```

Behaviour recovered from `background.js`:

- **Hooks every request**: `webRequest.onBeforeRequest`, `onBeforeSendHeaders`,
  `onHeadersReceived`, `onResponseStarted`, `onCompleted`, `onErrorOccurred`,
  `onBeforeRedirect` across `main_frame, sub_frame, object, image, xmlhttprequest, media, other`.
- **Sniffs server responses**: reads `Content-Type` and `Content-Disposition`, runs the
  MIME→extension table (below), and decides "IDM takes over" vs "let the browser have it".
- **Pauses the browser's own download** (`browser.downloads.pause`) and re-hands the URL to
  IDM — this is why downloads "disappear" from the browser into IDM.
- **Captures request context** to give IDM a replayable request: method, request headers,
  response headers, **cookies** (`browser.cookies.getAll`), `Referer`, `User-Agent`, and
  **proxy info** (`browser.proxy.settings`) translated into IDM's own proxy string format
  `1:<scheme>://<host>:<port>[;bypass,...]`.
- Uses **`declarativeNetRequest` session rules** to redirect selected requests to
  `captured.html?register=1#<url>` — i.e. it can silently replace a navigation.
- **Injects `content.js`** into all frames (`run_at: document_start`, `all_frames`) to scrape
  page media/links.
- **Video capture by monkey-patching XHR/fetch** in the page: the ROT-13-obfuscated string
  `Ya = ua("KZYUggcErdhrfg|bcra|fraq|...")` decodes to
  `XHRRequest|open|send|setRequestHeader|upload|onloadend|responseText|fetch|headers|body|text|json|getReader|pipeThrough|Headers|accept|*/*|application/vnd.t1c|Response|url|get`,
  and `ua("lbhghor")` decodes to **`youtube`**. So IDM ships per-site capture *rulesets*
  indexed by numeric site id; `case 1` matches `/watch?v=` (YouTube) and `case 4` matches
  `/p/` and `/reel/` (Instagram).
- Uses the pseudo-MIME **`application/vnd.t1c.*`** as a private side channel between injected
  page script and the background worker.
- Reads the user's **cookie/localStorage of IDM's own domain** for the client id
  (`idmwebext_cid`, `idmmzcc3_uri`) — a persistent install identifier.
- Opens its own **consent/upsell popup window** on install/update (`welcome.html?...&consent=1`).

### 4.7 Site-name obfuscation `[OBSERVED]`

`ua()` is ROT-13 with a wrap tweak (`(c & 31) > 13 ? -13 : 13`). It is applied only to a few
high-value literals — site names and the XHR-hook API list — presumably to slow down casual
"which sites do they support" grepping. It is trivially reversible.

### 4.8 MIME → extension table `[OBSERVED, excerpt]`

Shipped in the extension as `Aa` and mirrored server-side by `IDMFType.dat` +
`sdefextmap.dat`:

```
application/vnd.apple.mpegurl -> M3U8      video/mp2t      -> TS|TSV|M3U8
application/dash+xml          -> MPD       video/mp4       -> MP4|M4V|M4S
application/f4m+xml           -> F4M|MPD   video/vnd.mpeg.dash.mpd -> MPD
application/x-bittorrent      -> TORRENT|BTT   application/vnd.yt-ump  -> UMP
application/x-mpegurl         -> M3U8      text/plain      -> TXT|VTT|SRT|M3U|PLS|M3U8|MPD|F4M|TORRENT|BTT
```

Note `TORRENT|BTT` and `.torrent` are recognised **as file types** (the extension also lists
`magnet`, 2 hits) but there are **no BitTorrent protocol strings in `IDMan.exe`**
(`torrent` = 0 hits in the main binary). ⇒ `[INFERRED]` IDM can *download a `.torrent` file*
and maybe hand it off, but does **not** implement a BitTorrent client.

---

## 5. Streaming/media grabber `[OBSERVED]`

MSVC RTTI names recovered from `.rdata` (these are real class names, not marketing):

```
.?AVCM3UParser@@
.?AVdecodingState@CM3UParser@@
.?AVMasterPlaylist@CM3UParser@@
.?AVMediaPlaylist@CM3UParser@@
.?AVMediaSegment@CM3UParser@@
.?AVMPLVariant@CM3UParser@@
.?AVCMp4DashReader@@
.?AVCWebAuthorization@@
```

⇒ `CM3UParser` is a **complete HLS parser**: master playlist, media playlist, media segments,
and variant streams; `decodingState` implies incremental/buffered parsing. `CMp4DashReader`
covers DASH/ISO-BMFF. Supporting strings:

```
init_segment / InitSegment parsing error
#ANVATO-SEGMENT-INFO:                (Anvato platform, used by many US broadcasters)
-tmp_dashinit                        (temp file for a DASH init segment)
MPEG: Res. %dx%d, %.1f samp per sec, %d bits per samp
```

Defined video extensions (UI filter string):
`avi mpg mpe mpeg asf wmv mov qt rm mp4 flv m4v webm ogv ogg mkv ts tsv`

Related components: `idmvconv.dll` (converter), `IDMVMPrs*.dll` (parser), `idmvs.dll`
(video service), `IDMGrHlp.exe` (grabber subprocess).

**Pipeline `[INFERRED]`:** sniff a manifest → parse (`CM3UParser`/`CMp4DashReader`) → choose
variant by resolution/bitrate → download init segment + media segments concurrently using the
same `ChLst` engine → remux to MP4 → offer conversion. The `-tmp_dashinit` temp name confirms
the init segment is fetched separately and prefixed.

---

## 6. Site grabber `[OBSERVED]`

- Help file `grabber.chm`, helper `IDMGrHlp.exe`.
- Registry `HKCU\Software\DownloadManager\GrabberSts\{IncludeFilters, ExcludeFilters,
  Projects, RecentProjects, ...}`.
- On-disk: `sprojects.dat`, `sprojects2.dat` (project definitions), `surlexclist.dat`,
  `scnlurllist2.dat` (scan URL lists).
- `defexclist.txt` — 1,000+ shipped **exclude** patterns, plain wildcards, e.g.
  `http://counters.gigya.com/wildfire/counters/*=.tif`, `http://*.cloudfront.net/game/*/res/*.bin`.
  This is an anti-clutter list so the grabber ignores tracking pixels and CDN blobs.
- Also `Software\DownloadManager\GetAllDlgLS` ⇒ the "Get all links" dialog state.

⇒ The grabber is a recursive crawler with include/exclude URL filters, project persistence,
and a separate process for heavy scanning.

---

## 7. Configuration surface `[OBSERVED]`

Registry root: `HKCU\Software\DownloadManager\` with subkeys

```
ConfigTime   DwnlPanel   DwnlSelPanel   FoldersTree
GetAllDlgLS  GrabberDlgLS  GrabberSts\{IncludeFilters,ExcludeFilters,Projects,RecentProjects}
IDMBI\{Firefox,IEXPLORE}\DwnlPanel        ListSettings   maxID
Software\Backup_IDM                       (settings backup)
```

Browser discovery keys referenced:

```
Software\Mozilla\Firefox\Profiles\*
Software\Mozilla\{Mozilla Firefox,Firefox Developer Edition,Nightly,Waterfox,SeaMonkey}
Software\Mozilla\NativeMessagingHosts\com.tonec.idm
Software\Brave          Software\CocCoc\Browser\Extensions      (Cốc Cốc, VN)
Software\Classes\CLSID\ Software\Classes\PROTOCOLS\Name\Software\Classes\
```

Component paths are registered by running `regsvr32 /s "<path>"` on: `downlWithIDM64.dll`,
`IDMGetAll64.dll`, `IDMIECC64.dll`, `IDMShellExt*.dll` — all with a `/s` (silent) flag.

### Bundled Tor proxy presets `[OBSERVED]`

```
127.0.0.1:9050      (Tor SOCKS port)
127.0.0.1:9150      (Tor Browser SOCKS port)
```
IDM preconfigures these, which is why "download through Tor" is a one-click option.

---

## 8. Notable design decisions worth copying (and avoiding)

**Worth copying:**

1. **Custom HTTP stack** so range/resume/timeout policy is fully controllable.
2. **Conditional resume** with `If-Range` + `ETag`/`Last-Modified` — the correct, safe design.
3. **Dynamic chunk list** rather than a fixed up-front split — adapts to per-server throughput.
4. **Separate global / per-server / per-file limits** — genuinely useful and cheap.
5. **Localhost daemon + thin browser extension** — the extension stays dumb, the engine stays
   powerful, and the extension does not need to be rewritten when the engine changes.
6. **Two transports** (WebSocket on 127.0.0.1:1001 with native messaging as fallback) so that
   the capture works even when the socket is blocked.

**Worth avoiding:**

1. **Kernel filter drivers** (`idmwfp*.sys`, `idmtdi*.sys`) for traffic capture. They require
   elevation, break on OS updates, and are the usual cause of IDM's antivirus false positives
   and "downloads not captured" bug reports. A modern client should use the browser's own
   APIs (as the extension already largely does) instead.
2. **Spoofing the browser's proxy settings** (`browser.proxy.settings` + the `proxy`
   permission) just to observe traffic — invasive and fragile.
3. **PID-based trust in the native host** (`GetNamedPipeClientProcessId`) — a weak check; a
   local adversary can spoof the peer. An authenticated token handshake is better.
4. **Persistent install identifiers in the user's cookies** (`idmwebext_cid`, 2038 expiry).
5. **Silent navigation replacement** via `declarativeNetRequest` redirects.
6. **Renderer-process hooks via injected XHR/fetch patching** with per-site rule tables —
   these break every time a site changes and are a constant maintenance treadmill.

---

## 8b. Feature strings the engine analysis would have missed `[OBSERVED]`

A pass over the extracted strings (`ascii.txt` / `wide.txt`) against Tonec's advertised
feature list turns up whole features that no amount of reading the download engine would
reveal, because they are not in the engine. Recorded here as evidence; the Riptide status for
each is in `docs/FEATURE-MAP.md` §12.

| Feature | Strings |
| --- | --- |
| **MMS protocol** — genuinely implemented | `mms://`, `StImmMsg`, `mmsPacketSize`, `mmsIndex`, `send mms cmd`, `MMSCn:ProcRpl`, `Unk err in MMSCn:ProcWR`, `Unkerr in MMSC:SetCmpl`, `Live broacast flag is set in mms header!` (`MMSCn` = MMS connection class) |
| **Download quotas** (as distinct from rate limits) | `Download limits`, `Download limits exceeded!` |
| **Speed limiter** (the rate-limiting UI) | `Speed Limiter`, `Use Speed Limiter`, `Speed Limiter settings`, `Always turn on Speed Limiter on IDM startup`, `Remember Speed Limiter settings for this file`, `Show "Speed Limiter" tab`, `Connections and Limits` |
| **Periodic file synchronization** | `Periodic synchronization` (UTF-16) |
| **Scheduler power/dial-up actions** | `Shut down`, `Shut down now`, `Hang up modem`, `Hang up now`, `Internet Download Manager has finished queue processing. According to scheduler settings IDM is going to hang up your modem.` |
| **Antivirus scan on completion** | `If you would like to run antivirus software on download completion, you should type or browse the complete path to its executable file. If IDM knows about necessary parameters for a given executable file name, it will set them automatically.` plus the `[file]` placeholder convention |
| **Offline site archiving with link rewriting** | `Complete start page with all pictures, styles, and scripts for offline browsing`, `Convert the links in downloaded html files to the local files for offline browsing` |
| **FTP proxy** | `Use &FTP Proxy` |

The antivirus string doubles as an incidental confirmation of the merge-based completion
described in §2.4: *"IDM has received \"Access Denied\" error while assembling file parts… In
order to assemble this file, you will need to reboot your computer, select the file in IDM, and
press on resume button."* That is IDM telling the user to work around a failed final assembly —
the behaviour Riptide avoids by writing each range at its final offset and renaming.

---

## 9. Open questions `[UNKNOWN]`

- Exact chunk-splitting / re-splitting heuristic (`Change chunk size to %ld` algorithm).
- Exact timeout values for the two labelled timeout classes.
- The complete `MSG#` message-type table (I recovered the shape, the framing, the handshake,
  and many numeric opcodes ≥ 1 from the extension's dispatcher, but the server-side enum is
  not in the extension).
- Full per-site capture rulesets — the extension ships rule *slots* and indexes; the rule
  payloads are pushed from IDM at runtime (message type 18 with an obfuscated payload array).
- Whether the `0.1.0.1:1001` fallback address is a deliberate obfuscation of `127.0.0.1`.
  `[INFERRED]` yes — a trivially reversed dotted quad to deter naive scripted blocking.
- Wire details of the QAM/quic detection: the extension inspects `Client-Protocol: quic` and
  `Alternate-Protocol: ...:quic,p=0` and sets a flag bit (`b.W |= 16`), implying IDM's engine
  is aware of HTTP/3. `[INFERRED]`

---

## 10. How to reproduce this analysis

```bash
D="/mnt/c/Program Files (x86)/Internet Download Manager"

sha256sum "$D/IDMan.exe"
objdump -f -h  "$D/IDMan.exe"                 # PE32/i386, sections, start address
objdump -p     "$D/IDMan.exe" | grep "DLL Name"   # the 7 static imports

strings    -n 5 "$D/IDMan.exe" > ascii.txt    # 21,604 lines
strings -el -n 4 "$D/IDMan.exe" > wide.txt    # 1,950 lines  (UTF-16 resources)

grep -aE '^(Range|Content-Range|ETag|If-Range|Authorization|Proxy-Authorization)' ascii.txt
grep -aiE  'ChLst|CM3U|CMp4Dash|LoadLibrary|ws2_32|schannel|secur32' ascii.txt

# Extensions are ZIP after a CRX header; strip 12+header_len bytes for CRX3
python3 - <<'EOF'
import zipfile, io, struct
d = open("IDMGCExt.crx","rb").read()
n = struct.unpack('<I', d[8:12])[0]        # CRX3 header length
zipfile.ZipFile(io.BytesIO(d[12+n:])).extractall("IDMGCExt")
EOF
```

---

*All findings above are static; nothing here was obtained by executing IDM. Treat the
"avoid" list as engineering judgement, not a vulnerability report.*
