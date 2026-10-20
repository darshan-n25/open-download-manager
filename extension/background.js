// Open Download Manager browser integration.
//
// Talks to the local daemon over the same two channels IDM's own extension uses, minus the
// parts Open Download Manager deliberately drops:
//
//   * a WebSocket on 127.0.0.1:1001 negotiating `plugin.v3.internetdownloadmanager.com`,
//     speaking the `MSG#<id>#<type>#<a>#<b>[,k=v...];` framing;
//   * a plain JSON HTTP API on the same port, used by the popup for status.
//
// Deliberately *not* done, and why:
//   * no `declarativeNetRequest` redirect rules — cancelling in `downloads.onCreated` is
//     reversible and never silently rewrites traffic;
//   * no browser-proxy hijacking — Open Download Manager drives its own HTTP client;
//   * no native-messaging host — the WebSocket needs no per-OS host manifest, no registry
//     key, and no "trust anything that connects to the pipe" assumption.
//
// The socket is opened on demand and closed after a short idle period, so the service
// worker is not kept alive by a permanent connection.
//
// The wire framing lives in `protocol.js` (a classic script, so `importScripts` works in a
// Chrome service worker and a plain script tag works in Firefox) and is unit-tested by
// `tests/protocol.test.mjs`.

importScripts('./protocol.js');

const api = globalThis.browser ?? globalThis.chrome;

const { MSG, PROTOCOL_VERSION, WS_PROTOCOL, encodeFrame, drainFrames, daemonWsUrl } =
  globalThis.OdmProtocol;

// ---------------------------------------------------------------- configuration

const DAEMON = globalThis.OdmProtocol.DAEMON;
const API_BASE = `http://${DAEMON.host}:${DAEMON.port}`;

const IDLE_CLOSE_MS = 20_000;
const HANDSHAKE_TIMEOUT_MS = 4_000;
const REPLY_TIMEOUT_MS = 15_000;

const DEFAULTS = {
  /** Take over browser downloads and route them to Open Download Manager. */
  intercept: true,
  /** Empty means "every host"; otherwise only these hosts are intercepted. */
  allowHosts: [],
  /** Never intercept these hosts. */
  denyHosts: [],
  /** Never intercept these file extensions (browser handles them better). */
  denyExtensions: ['crx', 'xpi', 'pem', 'mobileconfig'],
  connections: 8,
  /** e.g. "2M"; empty means unlimited. */
  speedLimit: '',
  /** HLS/DASH preference: "best", "worst" or a height such as "720". */
  quality: 'best',
  /** Forward the browser's cookies so authenticated downloads keep working. */
  forwardCookies: true,
  /** Forward the page URL as Referer. */
  forwardReferer: true,
  /** Optional fixed output directory on the machine running the daemon. */
  outputDir: '',
  /**
   * The daemon's bearer token, when it requires one (empty = it does not).
   *
   * It rides in the WebSocket URL because a browser cannot set a header on a handshake.
   */
  daemonToken: '',
  proxy: '',
  auth: '',
  /** One `Name: value` per line. */
  extraHeaders: '',
  notify: true,
};

async function getSettings() {
  const stored = await api.storage.local.get('settings');
  return { ...DEFAULTS, ...(stored.settings || {}) };
}

async function setSettings(patch) {
  const current = await getSettings();
  const next = { ...current, ...patch };
  await api.storage.local.set({ settings: next });
  return next;
}

// ---------------------------------------------------------------- framing
//
// `encodeFrame`/`drainFrames` come from `protocol.js`, which mirrors `src/daemon.rs`.
// Components are percent-encoded; `encodeURIComponent` escapes the same delimiters
// (`%`, `,`, `;`, `=`, `#`) and every non-ASCII byte, so the Rust decoder reads back
// exactly what we sent.

// ---------------------------------------------------------------- daemon connection

let socket = null;
let opening = null;
let idleTimer = null;
let seq = 0;
const waiters = new Map();

function daemonUrl(settings) {
  return daemonWsUrl({
    host: DAEMON.host,
    port: DAEMON.port,
    cid: 'odm-ext',
    token: settings.daemonToken,
    nonce: Date.now(),
  });
}

function wsUrlForPopup(settings) {
  return daemonWsUrl({
    host: DAEMON.host,
    port: DAEMON.port,
    cid: 'odm-cli-popup',
    token: settings.daemonToken,
    nonce: Date.now(),
  });
}

function rejectAll(reason) {
  for (const waiter of waiters.values()) {
    clearTimeout(waiter.timer);
    waiter.reject(reason);
  }
  waiters.clear();
}

function onSocketMessage(event) {
  const ws = socket;
  // A previous message may have ended mid-frame; stitch it back on.
  const incoming = (ws && ws.partial ? ws.partial : '') + String(event.data);
  const { frames, rest } = drainFrames(incoming);
  if (ws) ws.partial = rest;
  for (const frame of frames) {
    const waiter = waiters.get(frame.id);
    if (!waiter) continue;
    clearTimeout(waiter.timer);
    waiters.delete(frame.id);
    waiter.resolve(frame);
  }
}

function onSocketClose() {
  socket = null;
  opening = null;
  setBadge(false);
  rejectAll(new Error('the Open Download Manager daemon closed the connection'));
}

/**
 * Close the live socket, if there is one.
 *
 * Used when the daemon token changes: the credential is presented in the handshake URL, so a
 * new value (or no value) only takes effect on a fresh connection.
 */
function closeSocket() {
  const ws = socket;
  socket = null;
  opening = null;
  if (!ws) return;
  try {
    ws.close();
  } catch {
    /* already gone */
  }
}

function scheduleIdleClose() {
  if (idleTimer) clearTimeout(idleTimer);
  idleTimer = setTimeout(() => {
    idleTimer = null;
    if (socket && waiters.size === 0) {
      try {
        socket.close();
      } catch {
        /* already gone */
      }
    }
  }, IDLE_CLOSE_MS);
}

/** Open (or reuse) a socket and complete the HELLO handshake. */
async function openSocket() {
  if (socket && socket.readyState === WebSocket.OPEN) return socket;
  if (opening) return opening;

  // Read the settings first: the daemon may require a bearer token, and a browser cannot set
  // a header on a WebSocket handshake, so the token has to be in the URL from the start.
  const settings = await getSettings();

  opening = new Promise((resolve, reject) => {
    let ws;
    try {
      ws = new WebSocket(daemonUrl(settings), WS_PROTOCOL);
    } catch (error) {
      opening = null;
      reject(error);
      return;
    }

    let settled = false;
    const fail = (message) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      opening = null;
      try {
        ws.close();
      } catch {
        /* ignore */
      }
      reject(new Error(message));
    };
    const timer = setTimeout(
      () => fail(`no answer from the Open Download Manager daemon on ${DAEMON.host}:${DAEMON.port}`),
      HANDSHAKE_TIMEOUT_MS,
    );

    ws.addEventListener('message', onSocketMessage);
    ws.addEventListener('close', onSocketClose);

    ws.addEventListener('open', () => {
      socket = ws;
      socket.partial = '';
      const id = ++seq;
      const handshake = new Promise((resolveHello, rejectHello) => {
        const t = setTimeout(() => rejectHello(new Error('handshake timed out')), HANDSHAKE_TIMEOUT_MS);
        waiters.set(id, {
          timer: t,
          resolve: resolveHello,
          reject: rejectHello,
        });
      });
      ws.send(
        encodeFrame({
          id,
          type: MSG.HELLO,
          a: PROTOCOL_VERSION,
          kv: {
            version: String(PROTOCOL_VERSION),
            client: `odm-extension/${api.runtime.getManifest().version}`,
            ua: navigator.userAgent,
          },
        }),
      );
      handshake
        .then((reply) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          opening = null;
          setBadge(true);
          scheduleIdleClose();
          resolve(ws);
          return reply;
        })
        .catch((error) => fail(error.message));
    });

    // `error` is always followed by `close`, which rejects the waiters.
    ws.addEventListener('error', () => fail('could not reach the Open Download Manager daemon'));
  });

  return opening;
}

/** Send one request and wait for the reply that echoes its message id. */
async function request(type, kv = {}) {
  const ws = await openSocket();
  scheduleIdleClose();
  return new Promise((resolve, reject) => {
    const id = ++seq;
    const timer = setTimeout(() => {
      waiters.delete(id);
      reject(new Error('the Open Download Manager daemon did not reply'));
    }, REPLY_TIMEOUT_MS);
    waiters.set(id, { timer, resolve, reject });
    ws.send(encodeFrame({ id, type, kv }));
  });
}

/** Queue a download, raising the daemon's own message on rejection. */
async function addDownload(kv) {
  const reply = await request(MSG.ADD, kv);
  if (reply.type === MSG.ERROR) {
    throw new Error(reply.kv.message || `daemon error ${reply.kv.code || '?'}`);
  }
  return reply.kv;
}

async function daemonStatus() {
  const reply = await request(MSG.STATUS);
  if (reply.type === MSG.ERROR) throw new Error('status failed');
  return reply.kv;
}

// ---------------------------------------------------------------- badge

function setBadge(connected) {
  const text = connected ? '' : '!';
  try {
    api.action.setBadgeText({ text });
    api.action.setBadgeBackgroundColor({ color: connected ? '#2f855a' : '#c53030' });
    api.action.setTitle({
      title: connected ? 'Open Download Manager — connected' : 'Open Download Manager — daemon not reachable',
    });
  } catch {
    /* the popup may not exist in some embedding hosts */
  }
}

// ---------------------------------------------------------------- download takeover

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return '';
  }
}

function extensionOf(url) {
  try {
    const path = new URL(url).pathname;
    const dot = path.lastIndexOf('.');
    const slash = path.lastIndexOf('/');
    if (dot < 0 || dot < slash) return '';
    return path.slice(dot + 1).toLowerCase();
  } catch {
    return '';
  }
}

function hostMatches(host, pattern) {
  const p = String(pattern).trim().toLowerCase().replace(/^\*\./, '');
  if (!p) return false;
  return host === p || host.endsWith(`.${p}`);
}

function shouldTakeOver(url, settings) {
  if (!/^https?:/i.test(url)) return false; // no data:, blob:, file:, chrome-extension:
  const host = hostOf(url);
  if (!host) return false;
  if (settings.denyHosts.some((p) => hostMatches(host, p))) return false;
  if (settings.allowHosts.length > 0 && !settings.allowHosts.some((p) => hostMatches(host, p))) {
    return false;
  }
  const ext = extensionOf(url);
  if (ext && settings.denyExtensions.map((e) => e.toLowerCase()).includes(ext)) return false;
  return true;
}

async function cookieHeader(url) {
  try {
    const cookies = await api.cookies.getAll({ url });
    if (!cookies || cookies.length === 0) return '';
    return cookies.map((c) => `${c.name}=${c.value}`).join('; ');
  } catch {
    return '';
  }
}

function notify(title, message) {
  try {
    api.notifications.create({ type: 'basic', title, message });
  } catch {
    /* notifications may be unavailable */
  }
}

/** Build the ADD payload shared by every entry point. */
async function payloadFor(url, { referer = '', settings, extra = {} } = {}) {
  const s = settings || (await getSettings());
  const kv = { url, ...extra };
  if (s.forwardReferer && referer) kv.referer = referer;
  if (s.forwardCookies) {
    const cookie = await cookieHeader(url);
    if (cookie) kv.cookie = cookie;
  }
  if (s.connections) kv.connections = String(s.connections);
  if (s.speedLimit) kv.speed_limit = s.speedLimit;
  if (s.quality) kv.quality = s.quality;
  if (s.outputDir) kv.output = `${s.outputDir.replace(/\/+$/, '')}/${filenameFor(url)}`;
  if (s.proxy) kv.proxy = s.proxy;
  if (s.auth) kv.auth = s.auth;
  if (s.extraHeaders) kv.headers_extra = s.extraHeaders;
  kv.user_agent = navigator.userAgent;
  return kv;
}

function filenameFor(url) {
  try {
    const base = new URL(url).pathname.split('/').filter(Boolean).pop() || 'download';
    return decodeURIComponent(base);
  } catch {
    return 'download';
  }
}

/** Send a URL to the daemon, returning its job descriptor. */
async function sendToDaemon(url, options = {}) {
  const kv = await payloadFor(url, options);
  return addDownload(kv);
}

api.downloads.onCreated.addListener(async (item) => {
  let settings;
  try {
    settings = await getSettings();
  } catch {
    return;
  }
  if (!settings.intercept) return;
  if (!shouldTakeOver(item.url, settings)) return;

  try {
    await api.downloads.cancel(item.id);
  } catch {
    return; // already finished, or not ours to cancel
  }
  try {
    await api.downloads.erase({ id: item.id });
  } catch {
    /* optional */
  }

  const referer = item.referrer && item.referrer !== 'about:blank' ? item.referrer : '';
  try {
    const kv = await sendToDaemon(item.url, { referer, settings });
    if (settings.notify) notify('Open Download Manager', `Queued ${kv.url || item.url}`);
  } catch (error) {
    if (settings.notify) notify('Open Download Manager could not take over the download', String(error.message || error));
    // Give the download back to the browser rather than losing it.
    try {
      api.downloads.download({ url: item.url });
    } catch {
      /* nothing more we can do */
    }
  }
});

// ---------------------------------------------------------------- context menu

async function installMenus() {
  try {
    await api.contextMenus.removeAll();
  } catch {
    /* first run */
  }
  const common = { title: 'Download with Open Download Manager' };
  api.contextMenus.create({ id: 'odm-link', contexts: ['link'], ...common });
  api.contextMenus.create({ id: 'odm-media', contexts: ['video', 'audio', 'image'], ...common });
  api.contextMenus.create({
    id: 'odm-page',
    title: 'Scan this page for media…',
    contexts: ['page'],
  });
}

api.runtime.onInstalled.addListener(installMenus);
api.runtime.onStartup.addListener(installMenus);

api.contextMenus.onClicked.addListener(async (info, tab) => {
  const settings = await getSettings();
  const referer = tab && tab.url ? tab.url : '';
  try {
    if (info.menuItemId === 'odm-link' || info.menuItemId === 'odm-media') {
      const kv = await sendToDaemon(info.linkUrl || info.srcUrl, { referer, settings });
      notify('Open Download Manager', `Queued ${kv.url || info.linkUrl || info.srcUrl}`);
    } else if (info.menuItemId === 'odm-page' && tab) {
      // The popup owns the interactive list; open it so the user can pick.
      await api.storage.local.set({ focusScan: true });
      if (api.action.openPopup) await api.action.openPopup();
    }
  } catch (error) {
    notify('Open Download Manager failed', String(error.message || error));
  }
});

// ---------------------------------------------------------------- keepalive

// A periodic, cheap round trip keeps the badge honest without holding the socket open.
api.alarms.create('odm-probe', { periodInMinutes: 1 });
api.alarms.onAlarm.addListener(async (alarm) => {
  if (alarm.name !== 'odm-probe') return;
  try {
    await daemonStatus();
    setBadge(true);
  } catch {
    setBadge(false);
  }
});

// ---------------------------------------------------------------- messages from the popup

api.runtime.onMessage.addListener((message, _sender, respond) => {
  (async () => {
    try {
      switch (message && message.kind) {
        case 'settings:get':
          respond({ ok: true, settings: await getSettings() });
          break;
        case 'settings:set': {
          const patch = message.patch || {};
          const settings = await setSettings(patch);
          // A token only matters at handshake time, so a changed one has to close the live
          // socket; otherwise the old credential would keep working until the socket went
          // idle, and a *removed* token would be equally invisible.
          if ('daemonToken' in patch) closeSocket();
          respond({ ok: true, settings });
          break;
        }
        case 'daemon:status':
          respond({ ok: true, kv: await daemonStatus() });
          break;
        case 'download':
          respond({ ok: true, kv: await sendToDaemon(message.url, { referer: message.referer }) });
          break;
        case 'download:many': {
          const results = [];
          for (const url of message.urls || []) {
            try {
              results.push({ url, ok: true, kv: await sendToDaemon(url, { referer: message.referer }) });
            } catch (error) {
              results.push({ url, ok: false, error: String(error.message || error) });
            }
          }
          respond({ ok: true, results });
          break;
        }
        case 'api:base':
          respond({
            ok: true,
            base: API_BASE,
            // The popup fetches the JSON API directly, so it needs the same credential.
            token: (await getSettings()).daemonToken,
          });
          break;
        default:
          respond({ ok: false, error: `unknown request ${message && message.kind}` });
      }
    } catch (error) {
      respond({ ok: false, error: String((error && error.message) || error) });
    }
  })();
  return true; // keep the channel open for the async reply
});

setBadge(false);
daemonStatus()
  .then(() => setBadge(true))
  .catch(() => setBadge(false));
