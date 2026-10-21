// Open Download Manager wire framing — the `MSG#` protocol shared by the extension and the daemon.
//
// This file is deliberately a *classic* script (no `export`/`import`) so that the same bytes
// work everywhere:
//
//   * Chrome MV3 service worker: `importScripts('./protocol.js')`
//   * Firefox background page:   listed in `background.scripts`
//   * Node (tests):              `require('./protocol.js')` / `import './protocol.js'`
//
// It is the JavaScript half of a two-implementation protocol, so it is covered by
// `tests/protocol.test.mjs` and by `values_encoded_the_way_encodeuricomponent_does_are_
// decoded_verbatim` on the Rust side.
//
// Frame shape:
//
//     MSG#<id>#<type>#<a>#<b>[,<key>=<value>…];
//
// Note there are exactly **four** `#` separators: `b` runs until the first `,` (the start of
// the key/value list) or the terminating `;`. Real frames from IDM's own message host look
// like `MSG#0#17#1#0:9:` — reading `b` up to a fifth `#` finds nothing and silently drops
// every reply.

(function (root) {
  'use strict';

  /** Opcodes. Open Download Manager's own numbering — see docs/FEATURE-MAP.md. */
  const MSG = {
    HELLO: 1,
    HELLO_ACK: 2,
    ADD: 3,
    ADD_ACK: 4,
    PING: 5,
    PONG: 6,
    STATUS: 7,
    STATUS_ACK: 8,
    ERROR: 9,
    BYE: 10,
  };

  const DAEMON = { host: '127.0.0.1', port: 1001 };
  const WS_PROTOCOL = 'plugin.v3.internetdownloadmanager.com';
  const PROTOCOL_VERSION = 16845059; // observed in the shipped IDM extension (= 0x01010903)

  function encodeComponent(value) {
    return encodeURIComponent(String(value));
  }

  function decodeComponent(text) {
    try {
      return decodeURIComponent(text);
    } catch {
      return text;
    }
  }

  /**
   * Build the daemon WebSocket URL.
   *
   * The browser cannot set a header on a WebSocket handshake, so when the daemon demands a
   * bearer token the token has to ride in the query string. A URL can end up in a log or in
   * shell history in a way a header does not, which is why the daemon accepts both forms and
   * prefers the header wherever a header is possible.
   */
  function daemonWsUrl({ host, port, cid, token, nonce }) {
    const base = `ws://${host}:${port}/?cid=${encodeComponent(cid)}&rnd=${nonce}`;
    return token ? `${base}&token=${encodeComponent(token)}` : base;
  }

  function encodeFrame(input) {
    const id = input.id;
    const type = input.type;
    const a = input.a === undefined ? 0 : input.a;
    const b = input.b === undefined ? 0 : input.b;
    let out = `MSG#${id}#${type}#${a}#${b}`;
    const pairs = Object.entries(input.kv || {}).filter(
      ([, v]) => v !== undefined && v !== null && String(v) !== '',
    );
    if (pairs.length > 0) {
      out += ',' + pairs.map(([k, v]) => `${encodeComponent(k)}=${encodeComponent(v)}`).join(',');
    }
    return out + ';';
  }

  /** Split one frame body (a string starting with `MSG#`, no trailing `;`) into fields. */
  function parseFrame(body) {
    const fields = [];
    let start = 0;
    // Four separators produce five fields, the last of which carries `b` plus any key/values.
    for (let i = 0; i < 4; i += 1) {
      const hash = body.indexOf('#', start);
      if (hash < 0) return null;
      fields.push(body.slice(start, hash));
      start = hash + 1;
    }
    if (fields[0] !== 'MSG') return null;

    const rest = body.slice(start);
    const comma = rest.indexOf(',');
    const bField = comma < 0 ? rest : rest.slice(0, comma);

    const kv = {};
    if (comma >= 0) {
      for (const pair of rest.slice(comma + 1).split(',')) {
        const eq = pair.indexOf('=');
        if (eq > 0) {
          kv[decodeComponent(pair.slice(0, eq))] = decodeComponent(pair.slice(eq + 1));
        }
      }
    }

    return {
      id: Number(fields[1]) || 0,
      type: Number(fields[2]) || 0,
      a: Number(fields[3]) || 0,
      b: Number(bField) || 0,
      kv,
    };
  }

  /**
   * Pull every complete frame out of a buffer.
   *
   * Returns `{ frames, rest }`, where `rest` is the unterminated tail the caller must
   * re-prepend to the next message. Leading garbage before the first `MSG#` is discarded.
   */
  function drainFrames(buffer) {
    const frames = [];
    let rest = String(buffer);
    for (;;) {
      const start = rest.indexOf('MSG#');
      if (start < 0) return { frames, rest: '' };
      rest = rest.slice(start);
      const end = rest.indexOf(';');
      if (end < 0) return { frames, rest };
      const parsed = parseFrame(rest.slice(0, end));
      rest = rest.slice(end + 1);
      if (parsed) frames.push(parsed);
    }
  }

  const OdmProtocol = {
    MSG,
    DAEMON,
    WS_PROTOCOL,
    PROTOCOL_VERSION,
    encodeComponent,
    decodeComponent,
    daemonWsUrl,
    encodeFrame,
    parseFrame,
    drainFrames,
  };

  root.OdmProtocol = OdmProtocol;
  // Node's CommonJS loader picks this up; browsers never define `module`.
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = OdmProtocol;
  }
})(typeof globalThis !== 'undefined' ? globalThis : this);
