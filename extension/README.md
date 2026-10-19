# Riptide browser integration

A Manifest V3 extension that hands downloads to the local `riptide daemon`. It is
intentionally small: the browser detects links and supplies session state, the daemon does
the downloading.

Not loaded automatically — install it as an unpacked extension (`chrome://extensions` →
*Developer mode* → *Load unpacked* → this directory; Firefox, `about:debugging` → *Load
Temporary Add-on* → pick `manifest.firefox.json`'s directory after renaming it, or keep a
copy named `manifest.json`).

## Files, and how to run the tests

| file | role |
| --- | --- |
| `manifest.json` | Chrome/Edge MV3 manifest (classic service worker) |
| `manifest.firefox.json` | same, with `background.scripts` and a Gecko id |
| `protocol.js` | the `MSG#` framing — the JavaScript half of a protocol implemented twice |
| `background.js` | the service worker: socket, takeover, context menu, settings |
| `popup.html` / `popup.js` / `popup.css` | the popup and the options page (also the daemon's controls via HTTP) |
| `tests/protocol.test.mjs` | unit tests for the framing, run with Node |

`protocol.js` is deliberately a **classic** script (no `import`/`export`) so that the same
file works in a Chrome service worker (`importScripts`), in a Firefox background page
(`background.scripts`), and in Node (`require`) for the tests.

```sh
node --test extension/tests/protocol.test.mjs
```

Those tests exist because the framing is implemented twice — once here, once in
`src/daemon.rs` — and the first version of the JavaScript parser looked for five `#`
separators where the protocol has four, so it silently dropped **every** reply the daemon
ever sent. The Rust side pins the same encoding in
`values_encoded_the_way_encodeuricomponent_does_are_decoded_verbatim`.

## Start the daemon first

```sh
riptide daemon                 # listens on 127.0.0.1:1001 by default
riptide daemon --print-endpoint
```

The badge stays on `!` until the daemon answers.

If the daemon was started with a token — `riptide daemon --token-file PATH`, the
`RIPTIDE_DAEMON_TOKEN` environment variable, or the `daemon_token_file` setting — put the same
value in the popup's **Daemon token** field under *Settings*. A browser cannot set a header on a
WebSocket handshake, so the extension sends it as `?token=…` on the socket URL and as an
`Authorization: Bearer` header on its HTTP reads; without it the daemon answers `401` and the
popup says so instead of claiming the daemon is offline. Changing the token drops the live socket,
because the credential is only presented at handshake time.

## What it does

- **Takes over browser downloads.** `downloads.onCreated` cancels the browser's own attempt
  and re-queues the URL with the daemon, carrying cookies, `Referer` and the browser's
  `User-Agent` so authenticated and hotlink-protected downloads still work. If the daemon is
  unreachable the download is handed back to the browser rather than lost.
- **Scans the active page** for `<video>`/`<audio>`/`<source>` sources and links to media,
  archives and manifests. `.m3u8`/`.mpd` candidates are tagged `stream` and captured as HLS
  or DASH.
- **Context menu**: "Download with Riptide" on any link, video, audio or image.
- **Shows the queue** with live progress, read from the daemon's HTTP API.

## Protocol

The extension speaks the same two channels the shipped IDM extension uses, minus the parts
Riptide deliberately drops.

**WebSocket** `ws://127.0.0.1:1001/?cid=…&rnd=…`, subprotocol
`plugin.v3.internetdownloadmanager.com`, handshake version `16845059` (`0x01010903`). When the
daemon requires a token the URL also carries `&token=…`.

**Framing** (as observed in IDM's own message host):

```
MSG#<msgId>#<type>#<a>#<b>[,<key>=<value>…];
```

Values are percent-encoded; `encodeURIComponent` escapes exactly the delimiters the daemon's
decoder expects (`%`, `,`, `;`, `=`, `#`).

| type | name | direction | payload |
| --- | --- | --- | --- |
| 1 | `HELLO` | ext → daemon | `version`, `client`, `ua` |
| 2 | `HELLO_ACK` | daemon → ext | `version`, `protocol`, `conn` (default connections per file), `perconn` (per-server ceiling, 0 = none), `maxconn` (largest `connections` an `ADD` may request), `hls`, `dash`, `ftp`, `server` |
| 3 | `ADD` | ext → daemon | `url`, `output`, `connections`, `quality`, `kind`, `referer`, `cookie`, `user_agent`, `proxy`, `auth`, `speed_limit`, `headers_extra` |
| 4 | `ADD_ACK` | daemon → ext | `id`, `url`, `next` |
| 5 / 6 | `PING` / `PONG` | both | — |
| 7 / 8 | `STATUS` / `STATUS_ACK` | both | `total`, `active`, `queued`, `slots` |
| 9 | `ERROR` | daemon → ext | `code`, `message` |
| 10 | `BYE` | ext → daemon | — |

`ADD` is the only message that starts work. `kind` is optional: when omitted the daemon
infers `hls`/`dash`/`ftp`/`http` from the URL.

The daemon's opcode *numbers* are Riptide's own. IDM's server-side enumeration is not present
in its shipped extension (only the dispatcher is), so the numbers are not claimed to match
IDM's; the framing, the handshake and the subprotocol string are.

**HTTP API** on the same port, used by the popup:

| method | path | purpose |
| --- | --- | --- |
| `GET` | `/` | HTML control page |
| `GET` | `/health` | `ok` |
| `GET` | `/api/jobs` | all jobs as JSON |
| `GET` | `/api/jobs/{id}` | one job |
| `POST` | `/api/jobs` | `{"url": …}` plus any `ADD` key → `201` with the job |

Every route requires the daemon's token when one is configured, as `Authorization: Bearer …`
(for a script) or `?token=…` (for a browser). A `POST` must send
`Content-Type: application/json`: browsers preflight that media type, and the daemon answers no
preflight, which is what keeps a page on another origin from queueing downloads on a tokenless
daemon.

## Permissions, and why

| permission | reason |
| --- | --- |
| `downloads` | observe and cancel the browser's own download so Riptide can take over |
| `cookies` | forward the session cookies the page would have sent, so authenticated downloads work |
| `contextMenus` | the "Download with Riptide" entries |
| `scripting`, `activeTab` | scan the current page for media, only when you press **Scan** |
| `storage` | settings |
| `notifications` | report queued/failed downloads |
| `alarms` | a once-a-minute status probe for the badge |
| `<all_urls>` | cookies and takeover need to work on whatever site you download from |

No `webRequest` blocking listener, no `declarativeNetRequest` redirect rules, no proxy
configuration is touched, and no native messaging host is installed.

## Deliberate differences from IDM's extension

| IDM | Riptide | Why |
| --- | --- | --- |
| Redirects requests via `declarativeNetRequest` rules so the browser's download never starts | Cancels in `downloads.onCreated` | Reversible, visible in the downloads UI, no silent traffic rewriting |
| Installs `IDMMsgHost.exe` as a native messaging host and trusts anything reaching its named pipe | WebSocket on loopback | No per-OS host manifest or registry key, and the peer is an actual TCP endpoint |
| Points the browser at IDM's own proxy to capture traffic | Nothing — Riptide drives its own HTTP stack | No interception of unrelated browsing |
| Trust is established from the peer PID | Optional bearer token; loopback-only with no token | A malicious local process could queue downloads either way; this at least cannot redirect traffic. Off loopback the daemon asks for a shared secret instead of inferring an identity, and refuses a `POST` whose content type a cross-origin page could have produced |
