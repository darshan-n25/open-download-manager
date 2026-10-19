// Tests for the extension's half of the `MSG#` protocol.
//
// Run with:  node --test extension/tests/protocol.test.mjs
//
// These exist because the framing is implemented twice — once here, once in
// `src/daemon.rs` — and the two must agree byte for byte. The first version of
// `parseFrame` looked for five `#` separators and therefore dropped every single reply the
// daemon ever sent; a browser is a slow place to discover that.

import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const P = require('../protocol.js');
const { MSG, encodeFrame, parseFrame, drainFrames, PROTOCOL_VERSION } = P;

test('the shipped IDM sample frame parses', () => {
  // Taken verbatim from IDMMsgHost.exe. Four `#` separators, and a `b` field of `0:9:`.
  const frame = parseFrame('MSG#0#17#1#0:9:');
  assert.deepEqual(frame, { id: 0, type: 17, a: 1, b: 0, kv: {} });
});

test('a frame with key/values round-trips', () => {
  const sent = {
    id: 7,
    type: MSG.ADD,
    kv: { url: 'https://host/a b.iso', cookie: 'a=1;b=2', user_agent: 'Mozilla/5.0' },
  };
  const text = encodeFrame(sent);
  assert.ok(text.startsWith('MSG#7#3#0#0,'), text);
  assert.ok(text.endsWith(';'), text);

  const body = text.slice(0, -1);
  const back = parseFrame(body);
  assert.equal(back.id, 7);
  assert.equal(back.type, MSG.ADD);
  assert.deepEqual(back.kv, sent.kv);
});

test('a frame with no key/values round-trips', () => {
  const back = parseFrame(encodeFrame({ id: 2, type: MSG.PONG }).slice(0, -1));
  assert.deepEqual(back, { id: 2, type: 6, a: 0, b: 0, kv: {} });
});

test('delimiters survive inside values', () => {
  const value = 'https://h/p?q=a=b&r=c,d;e#f%g';
  const text = encodeFrame({ id: 1, type: MSG.ADD, kv: { url: value } });
  // No raw framing character may appear inside the value.
  const body = text.slice('MSG#1#3#0#0,url='.length, -1);
  assert.ok(!/[;,=#]/.test(body.replace(/%[0-9A-F]{2}/g, '')), body);
  assert.equal(parseFrame(text.slice(0, -1)).kv.url, value);
});

test('non-ASCII values survive', () => {
  const value = 'füße — 日本語 — 😀';
  const text = encodeFrame({ id: 1, type: MSG.ADD, kv: { output: value } });
  assert.equal(parseFrame(text.slice(0, -1)).kv.output, value);
});

test('empty values are omitted rather than sent as empty keys', () => {
  const text = encodeFrame({ id: 1, type: MSG.ADD, kv: { url: 'https://h/x', proxy: '' } });
  assert.ok(!text.includes('proxy'), text);
  assert.equal(parseFrame(text.slice(0, -1)).kv.url, 'https://h/x');
});

test('several frames in one message are all returned', () => {
  const buffer =
    encodeFrame({ id: 1, type: MSG.PING }) +
    encodeFrame({ id: 2, type: MSG.STATUS }) +
    encodeFrame({ id: 3, type: MSG.BYE });
  const { frames, rest } = drainFrames(buffer);
  assert.equal(rest, '');
  assert.deepEqual(
    frames.map((f) => [f.id, f.type]),
    [
      [1, MSG.PING],
      [2, MSG.STATUS],
      [3, MSG.BYE],
    ],
  );
});

test('a partial frame is held back and completed later', () => {
  const whole = encodeFrame({ id: 9, type: MSG.HELLO_ACK, kv: { server: 'riptide/0.1.0' } });
  const cut = 12;

  const first = drainFrames(whole.slice(0, cut));
  assert.equal(first.frames.length, 0);
  assert.equal(first.rest, whole.slice(0, cut));

  const second = drainFrames(first.rest + whole.slice(cut));
  assert.equal(second.frames.length, 1);
  assert.equal(second.frames[0].kv.server, 'riptide/0.1.0');
  assert.equal(second.rest, '');
});

test('leading garbage is discarded', () => {
  const { frames } = drainFrames('\r\nnoise\n' + encodeFrame({ id: 4, type: MSG.PONG }));
  assert.equal(frames.length, 1);
  assert.equal(frames[0].id, 4);
});

test('a truncated header yields no frames and no crash', () => {
  for (const junk of ['MSG#', 'MSG#1#2#', 'MSG#1#2#0#', 'not a frame at all']) {
    const { frames } = drainFrames(junk);
    assert.deepEqual(frames, [], junk);
  }
});

test('the real HELLO_ACK the daemon sends parses completely', () => {
  // Captured from a live daemon during the smoke test.
  const raw =
    'MSG#1#2#0#0,version=16845059,protocol=plugin.v3.internetdownloadmanager.com,' +
    'conn=8,perconn=2,maxconn=32,hls=1,dash=1,ftp=1,server=riptide/0.1.0;';
  const { frames, rest } = drainFrames(raw);
  assert.equal(rest, '');
  assert.equal(frames.length, 1);
  const f = frames[0];
  assert.equal(f.id, 1);
  assert.equal(f.type, MSG.HELLO_ACK);
  assert.equal(f.kv.protocol, 'plugin.v3.internetdownloadmanager.com');
  assert.equal(f.kv.perconn, '2');
  assert.equal(f.kv.server, 'riptide/0.1.0');
  // The daemon answers with the same packed constant the extension announced.
  assert.equal(f.kv.version, String(PROTOCOL_VERSION));
});

test('the version constant is the one the shipped IDM extension sends', () => {
  // Observed as a decimal literal in the shipped extension's script. Spelled as the decimal,
  // because that is the evidence; the hex is a derived reading of it.
  assert.equal(PROTOCOL_VERSION, 16845059);
  assert.equal(PROTOCOL_VERSION, 0x01010903);
  assert.notEqual(PROTOCOL_VERSION, 0x01010d03); // 16846083 — an easy off-by-one-hex-digit trap
});

test('encoding is exactly encodeURIComponent', () => {
  // The Rust decoder is asserted against these same strings in
  // `src/daemon.rs::values_encoded_the_way_encodeuricomponent_does_are_decoded_verbatim`.
  assert.equal(P.encodeComponent('a=1; b=2; c=3'), 'a%3D1%3B%20b%3D2%3B%20c%3D3');
  assert.equal(P.encodeComponent('/home/me/My Files/x.iso'), '%2Fhome%2Fme%2FMy%20Files%2Fx.iso');
});

test('the daemon URL carries a token only when there is one', () => {
  const base = { host: '127.0.0.1', port: 1001, cid: 'riptide-ext', nonce: 1234 };

  const bare = P.daemonWsUrl({ ...base, token: '' });
  assert.equal(bare, 'ws://127.0.0.1:1001/?cid=riptide-ext&rnd=1234');
  assert.ok(!bare.includes('token'), bare);

  // Undefined and null behave like empty, so a settings file that predates the field works.
  assert.equal(P.daemonWsUrl({ ...base, token: undefined }), bare);
  assert.equal(P.daemonWsUrl({ ...base, token: null }), bare);

  const withToken = P.daemonWsUrl({ ...base, token: 'abc123' });
  assert.equal(withToken, 'ws://127.0.0.1:1001/?cid=riptide-ext&rnd=1234&token=abc123');
});

test('a token that needs encoding is encoded', () => {
  // A hex token needs none, but nothing stops a deployment from using a passphrase, and an
  // unencoded `&` or `#` would silently truncate the query the daemon parses.
  const url = P.daemonWsUrl({
    host: '10.0.0.5',
    port: 1001,
    cid: 'riptide-ext',
    token: 'a&b=c #d',
    nonce: 7,
  });
  assert.equal(url, 'ws://10.0.0.5:1001/?cid=riptide-ext&rnd=7&token=a%26b%3Dc%20%23d');

  // And the daemon's own parser reads back exactly what was sent: percent-decode, then take
  // the `token=` pair. (The Rust side is asserted in `src/daemon.rs::a_token_query_parameter
  // _is_read_and_decoded`.)
  const query = url.split('?')[1];
  const pairs = query.split('&');
  const value = pairs.find((p) => p.startsWith('token=')).slice('token='.length);
  assert.equal(decodeURIComponent(value), 'a&b=c #d');
  // Splitting on `&` must not have broken the token in two.
  assert.equal(pairs.length, 3);
});
