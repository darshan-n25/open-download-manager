// Riptide popup.
//
// Status and the job list come from the daemon's plain JSON HTTP API; everything that
// mutates state goes through the background page, so there is exactly one WebSocket client
// and one place that owns the MSG# protocol.

const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);

let apiBase = 'http://127.0.0.1:1001';
let found = [];
let refreshTimer = null;

// ---------------------------------------------------------------- plumbing

function bg(kind, payload = {}) {
  return new Promise((resolve, reject) => {
    api.runtime.sendMessage({ kind, ...payload }, (reply) => {
      const error = api.runtime.lastError;
      if (error) {
        reject(new Error(error.message));
        return;
      }
      if (!reply || !reply.ok) {
        reject(new Error((reply && reply.error) || 'the background page did not answer'));
        return;
      }
      resolve(reply);
    });
  });
}

function escapeHtml(value) {
  return String(value).replace(
    /[&<>"']/g,
    (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c],
  );
}

function formatBytes(n) {
  if (n === null || n === undefined) return '?';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(2)} ${units[i]}`;
}

function basename(url) {
  try {
    const path = new URL(url).pathname;
    return decodeURIComponent(path.split('/').filter(Boolean).pop() || url);
  } catch {
    return url;
  }
}

// ---------------------------------------------------------------- daemon status

function setStatus(online) {
  const pill = $('status');
  pill.textContent = online ? 'connected' : 'offline';
  pill.className = `pill ${online ? 'pill-on' : 'pill-off'}`;
  $('offline-hint').classList.toggle('hidden', online);
  $('endpoint').textContent = apiBase;
}

async function loadJobs() {
  let jobs;
  try {
    const response = await fetch(`${apiBase}/api/jobs`, { cache: 'no-store' });
    jobs = await response.json();
  } catch {
    setStatus(false);
    $('queue-summary').textContent = 'the daemon is not reachable';
    $('jobs').innerHTML = '';
    return;
  }
  setStatus(true);

  if (!Array.isArray(jobs) || jobs.length === 0) {
    $('queue-summary').textContent = 'nothing queued yet';
    $('jobs').innerHTML = '';
    return;
  }

  const count = (state) => jobs.filter((j) => j.state === state).length;
  const active = count('active');
  const done = count('done');
  const failed = count('failed');
  const waiting = jobs.filter((j) => ['queued', 'scheduled', 'paused'].includes(j.state)).length;
  $('queue-summary').textContent =
    `${active} active · ${waiting} waiting · ${done} done` + (failed ? ` · ${failed} failed` : '');

  const recent = jobs
    .slice()
    .sort((a, b) => (b.created_ms || 0) - (a.created_ms || 0))
    .slice(0, 25);

  $('jobs').innerHTML = recent
    .map((job) => {
      const label = basename(job.output || job.url || '');
      const pct = job.total ? Math.min(100, Math.round((job.downloaded / job.total) * 100)) : null;
      const detail =
        pct === null
          ? job.error || job.url
          : `${pct}% of ${formatBytes(job.total)}${job.error ? ` — ${job.error}` : ''}`;
      const bar = pct === null ? '' : `<div class="bar"><i style="width:${pct}%"></i></div>`;
      return `<li>
        <div style="flex:1;min-width:0">
          <div class="name" title="${escapeHtml(job.url || '')}">${escapeHtml(label)}</div>
          <div class="small muted">${escapeHtml(detail)}</div>
          ${bar}
        </div>
        <span class="state ${escapeHtml(job.state)}">${escapeHtml(job.state)}</span>
      </li>`;
    })
    .join('');
}

// ---------------------------------------------------------------- page scan
//
// `collectMedia` is injected into the page, so it must be self-contained: no references to
// anything in this file.

function collectMedia() {
  const out = new Map();
  const add = (raw) => {
    if (!raw || typeof raw !== 'string') return;
    if (!/^https?:/i.test(raw)) return;
    try {
      out.set(new URL(raw, document.baseURI).href, true);
    } catch {
      /* not a usable URL */
    }
  };

  const STREAM = /\.(m3u8|mpd)(\?|#|$)/i;
  const MEDIA = /\.(mp4|m4v|mkv|mov|webm|flv|avi|mp3|m4a|aac|flac|wav|ogg|opus)(\?|#|$)/i;
  const FILE =
    /\.(zip|rar|7z|tar|gz|bz2|xz|iso|img|dmg|pkg|deb|rpm|apk|exe|msi|pdf|epub|bin)(\?|#|$)/i;

  document.querySelectorAll('video, audio').forEach((el) => {
    add(el.currentSrc);
    add(el.src);
    el.querySelectorAll('source[src]').forEach((s) => add(s.src));
  });
  document.querySelectorAll('source[src], track[src]').forEach((s) => add(s.src));
  document.querySelectorAll('a[href]').forEach((a) => {
    const href = a.getAttribute('href') || '';
    if (STREAM.test(href) || MEDIA.test(href) || FILE.test(a.href)) add(a.href);
  });

  return [...out.keys()];
}

function renderMedia() {
  $('media').innerHTML = found
    .map((url, index) => {
      const isStream = /\.(m3u8|mpd)(\?|#|$)/i.test(url);
      return `<li>
        <input type="checkbox" data-index="${index}" checked />
        <span title="${escapeHtml(url)}">${escapeHtml(basename(url))}${
          isStream ? ' <em class="muted">stream</em>' : ''
        }</span>
      </li>`;
    })
    .join('');
  updateSendButton();
}

function selectedUrls() {
  return [...$('media').querySelectorAll('input[type=checkbox]')]
    .filter((box) => box.checked)
    .map((box) => found[Number(box.dataset.index)]);
}

function updateSendButton() {
  $('send-selected').disabled = selectedUrls().length === 0;
}

async function activeTab() {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });
  return tab;
}

async function scanPage() {
  $('scan').disabled = true;
  $('scan-note').textContent = 'Scanning…';
  try {
    const tab = await activeTab();
    if (!tab || !tab.id || !/^https?:/i.test(tab.url || '')) {
      $('scan-note').textContent = 'This tab has nothing to scan.';
      found = [];
      renderMedia();
      return;
    }
    const [injection] = await api.scripting.executeScript({
      target: { tabId: tab.id },
      func: collectMedia,
    });
    found = Array.isArray(injection && injection.result) ? injection.result : [];
    $('scan-note').textContent =
      found.length === 0
        ? 'No video, audio or archive links found on this page.'
        : `${found.length} candidate${found.length === 1 ? '' : 's'} — untick anything you do not want.`;
    renderMedia();
  } catch (error) {
    $('scan-note').textContent = `Scan failed: ${error.message}`;
  } finally {
    $('scan').disabled = false;
  }
}

async function sendSelected() {
  const urls = selectedUrls();
  if (urls.length === 0) return;
  const tab = await activeTab();
  const referer = tab && tab.url ? tab.url : '';

  $('send-selected').disabled = true;
  try {
    const reply = await bg('download:many', { urls, referer });
    const failed = reply.results.filter((r) => !r.ok);
    $('scan-note').textContent =
      failed.length === 0
        ? `Queued ${reply.results.length} download${reply.results.length === 1 ? '' : 's'}.`
        : `${reply.results.length - failed.length} queued, ${failed.length} failed — ${failed[0].error}`;
    await loadJobs();
  } catch (error) {
    $('scan-note').textContent = `Could not queue: ${error.message}`;
  } finally {
    updateSendButton();
  }
}

// ---------------------------------------------------------------- manual URL

async function addManual() {
  const input = $('manual-url');
  const url = input.value.trim();
  if (!url) return;
  $('manual-add').disabled = true;
  try {
    const tab = await activeTab();
    await bg('download', { url, referer: tab && tab.url ? tab.url : '' });
    input.value = '';
    $('manual-note').textContent = 'Queued.';
    await loadJobs();
  } catch (error) {
    $('manual-note').textContent = `Failed: ${error.message}`;
  } finally {
    $('manual-add').disabled = false;
  }
}

// ---------------------------------------------------------------- settings

const SETTINGS_FIELDS = [
  ['set-intercept', 'intercept', 'checkbox'],
  ['set-connections', 'connections', 'number'],
  ['set-speed', 'speedLimit', 'text'],
  ['set-quality', 'quality', 'text'],
  ['set-output', 'outputDir', 'text'],
  ['set-cookies', 'forwardCookies', 'checkbox'],
  ['set-referer', 'forwardReferer', 'checkbox'],
  ['set-notify', 'notify', 'checkbox'],
  ['set-allow', 'allowHosts', 'list'],
  ['set-deny', 'denyHosts', 'list'],
  ['set-proxy', 'proxy', 'text'],
  ['set-auth', 'auth', 'text'],
  ['set-headers', 'extraHeaders', 'text'],
];

function readForm() {
  const patch = {};
  for (const [id, key, type] of SETTINGS_FIELDS) {
    const el = $(id);
    if (type === 'checkbox') patch[key] = el.checked;
    else if (type === 'number') patch[key] = Number(el.value) || 1;
    else if (type === 'list') {
      patch[key] = el.value
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean);
    } else patch[key] = el.value.trim();
  }
  return patch;
}

function writeForm(settings) {
  for (const [id, key, type] of SETTINGS_FIELDS) {
    const el = $(id);
    if (type === 'checkbox') el.checked = Boolean(settings[key]);
    else if (type === 'list') el.value = (settings[key] || []).join(', ');
    else el.value = settings[key] === undefined || settings[key] === null ? '' : String(settings[key]);
  }
}

async function loadSettings() {
  const reply = await bg('settings:get');
  writeForm(reply.settings);
}

function bindSettings() {
  let saveTimer = null;
  const save = async () => {
    try {
      await bg('settings:set', { patch: readForm() });
      $('settings-note').textContent = 'Saved.';
    } catch (error) {
      $('settings-note').textContent = `Could not save: ${error.message}`;
    }
  };
  // Text inputs fire on every keystroke; debounce so a sentence is one write.
  const debounced = () => {
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(save, 400);
  };
  for (const [id, , type] of SETTINGS_FIELDS) {
    const el = $(id);
    if (type === 'text') el.addEventListener('input', debounced);
    el.addEventListener('change', save);
  }
}

// ---------------------------------------------------------------- wiring

async function init() {
  try {
    apiBase = (await bg('api:base')).base;
  } catch {
    /* keep the default endpoint */
  }
  setStatus(false);
  $('endpoint').textContent = apiBase;

  // The options page is this same markup with `#options`.
  if (location.hash === '#options') $('options').open = true;

  $('scan').addEventListener('click', scanPage);
  $('refresh').addEventListener('click', loadJobs);
  $('media').addEventListener('change', updateSendButton);
  $('send-selected').addEventListener('click', sendSelected);
  $('manual-add').addEventListener('click', addManual);
  $('manual-url').addEventListener('keydown', (event) => {
    if (event.key === 'Enter') addManual();
  });

  await loadSettings().catch(() => undefined);
  bindSettings();
  await loadJobs();

  refreshTimer = setInterval(loadJobs, 1000);
  window.addEventListener('unload', () => clearInterval(refreshTimer));

  const { focusScan } = await api.storage.local.get('focusScan');
  if (focusScan) {
    await api.storage.local.remove('focusScan');
    await scanPage();
  }
}

init();
