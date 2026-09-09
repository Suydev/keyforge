/* app.js — dashboard controller.
 *
 * BATTERY (target <=5%/hour on a Pad Go)
 *  - SSE push, never poll. The server only sends when state actually changed.
 *  - The stream is CLOSED when the tab is hidden and reopened on return, so a
 *    backgrounded dashboard costs nothing at all.
 *  - Only the visible tab re-renders. Switching tabs renders on demand.
 *  - No animation loop anywhere; charts redraw on data change only.
 *
 * RESILIENCE
 *  - Reconnect is immediate on `online`, otherwise backs off 1s -> 15s.
 *  - Offline is shown as "requests are held", because that is what the gateway
 *    actually does — it does not fail them.
 */
'use strict';

import {
  LineChart, Chime, themeChanged,
  fmtBytes, fmtInt, fmtMoney, fmtDuration, fmtClock, fmtAgo,
} from './charts.js';

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));
const el = (tag, cls, text) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
};
// Escape a string for embedding inside an HTML attribute or text in a
// template literal. Settings values are user-editable and round-trip through
// innerHTML, so anything from the server file is untrusted markup otherwise.
const esc = (v) => String(v ?? '').replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

/* ══ RANGE MODEL ═══════════════════════════════════════════════════════════
 * A range maps to (resolution, bucket count). The resolution decides WHICH
 * server-side rollup is queried, so a 30-day chart reads 720 hour-buckets
 * instead of thinning 43,200 minute-buckets in the browser.
 *
 * `live` marks the ranges that the SSE snapshot can satisfy on its own — those
 * update with every push and never issue a fetch. Wider ranges are pulled once
 * on selection and then refreshed lazily, because a 30-day chart does not change
 * meaningfully between two pushes.
 * ═══════════════════════════════════════════════════════════════════════════ */

const RANGES = {
  '5m':  { res: 'minute', buckets: 5,    label: '1m buckets',  live: true },
  '30m': { res: 'minute', buckets: 30,   label: '1m buckets',  live: true },
  '2h':  { res: 'minute', buckets: 120,  label: '1m buckets',  live: true },
  '6h':  { res: 'minute', buckets: 360,  label: '1m buckets',  live: false },
  '24h': { res: 'hour',   buckets: 24,   label: '1h buckets',  live: false },
  '7d':  { res: 'hour',   buckets: 168,  label: '1h buckets',  live: false },
  '30d': { res: 'day',    buckets: 30,   label: '1d buckets',  live: false },
  'all': { res: 'day',    buckets: 9999, label: '1d buckets',  live: false },
};

/** Refetch a non-live range at most this often. */
const HISTORY_TTL_MS = 60_000;

/* ══ DOM RECONCILIATION ════════════════════════════════════════════════════
 * Why this exists: every render function used to do `innerHTML = ''` and rebuild.
 * That has three costs the user actually feels —
 *
 *   1. entrance animations replay on every push, so cards visibly flash every
 *      few seconds (the "reloading whole animation every 5 sec" complaint);
 *   2. any transient UI state inside a row is destroyed — focus, text selection,
 *      an open <details>, scroll position in a nested container;
 *   3. it allocates and lays out the entire subtree instead of touching the two
 *      numbers that actually changed.
 *
 * So: identify rows by a stable key, create only what is new, patch what exists,
 * remove what is gone. Entrance animation is applied ONLY on creation.
 * ═══════════════════════════════════════════════════════════════════════════ */

/**
 * Write text only when it differs.
 *
 * Assigning identical textContent still dirties the node in some engines, and it
 * restarts any CSS transition keyed on content. Cheap guard, real effect.
 */
function setText(node, text) {
  const s = String(text);
  if (node.textContent !== s) node.textContent = s;
}

/** Toggle a class only when the state actually flips (same reason as setText). */
function setClass(node, cls, on) {
  if (on) {
    if (!node.classList.contains(cls)) node.classList.add(cls);
  } else if (node.classList.contains(cls)) {
    node.classList.remove(cls);
  }
}

/**
 * Keyed list reconciliation.
 *
 * @param {Element} parent      container whose children mirror `items`
 * @param {Array}   items       data, in the order it should appear
 * @param {(item, i) => string} keyOf   stable identity per item
 * @param {(item, i) => Element} create builds a fresh node (gets the entrance anim)
 * @param {(node, item, i) => void} update patches an existing node in place
 */
function reconcile(parent, items, keyOf, create, update) {
  const existing = new Map();
  for (const node of Array.from(parent.children)) {
    const k = node.dataset.k;
    if (k != null && !existing.has(k)) existing.set(k, node);
    else node.remove(); // duplicate or unkeyed leftover
  }

  let cursor = null; // node the next item must follow
  items.forEach((item, i) => {
    const k = String(keyOf(item, i));
    let node = existing.get(k);
    if (node) {
      existing.delete(k);
      update(node, item, i);
    } else {
      node = create(item, i);
      node.dataset.k = k;
      // `create` only builds structure; `update` is what writes values. Calling
      // it here too is essential — without it a brand-new row renders with empty
      // cells and stays that way until the NEXT push happens to arrive.
      update(node, item, i);
      // Only NEW rows animate in. This is the whole point: an existing row that
      // merely changed value must not replay its entrance.
      //
      // The class is dropped once the animation finishes. That matters for the
      // leaderboard, which reorders by score: `insertBefore` on a connected node
      // is a move, and a move restarts a running CSS animation — so a provider
      // changing rank would flash again every time it moved.
      setClass(node, 'fade-in', true);
      node.style.setProperty('--i', String(Math.min(i, 10)));
      node.addEventListener(
        'animationend',
        () => setClass(node, 'fade-in', false),
        { once: true },
      );
    }
    // Place it after the previous item, but only if it is not already there —
    // an unnecessary insertBefore is a real move and restarts animations.
    const want = cursor ? cursor.nextSibling : parent.firstChild;
    if (node !== want) parent.insertBefore(node, want);
    cursor = node;
  });

  // Anything still in the map is gone from the data.
  for (const node of existing.values()) node.remove();
}

/**
 * Build a row of cells once, then hand back setters for the volatile ones.
 *
 * Keeps `create` and `update` from drifting apart: the cell order is declared in
 * exactly one place instead of duplicated across two functions that must agree.
 */
function cells(tr, defs) {
  for (const d of defs) {
    const td = el('td', d.cls || null);
    td.dataset.f = d.f;
    tr.appendChild(td);
  }
}

/** Patch the cells of a row built by `cells()`. */
function patchCells(tr, values) {
  for (const [field, value] of Object.entries(values)) {
    const td = tr.querySelector(`[data-f="${field}"]`);
    if (td) setText(td, value);
  }
}

/* ── icons (inline SVG, never emoji) ─────────────────────────────────────── */

const ICON = {
  check: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6 9 17l-5-5"/></svg>',
  x: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M18 6 6 18M6 6l12 12"/></svg>',
  warn: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/></svg>',
  info: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10"/><path d="M12 16v-4M12 8h.01"/></svg>',
};

const PROVIDER_COLOR = { tabi: '#38bdf8', gorouter: '#a78bfa' };
const colorFor = (id) => PROVIDER_COLOR[id] || '#22c55e';

/* ══ DISPLAY SETTINGS ══════════════════════════════════════════════════════
 * Persisted to localStorage and applied as data-attributes on <html>, which is
 * why the CSS can express every mode as a token reassignment. The same values
 * are read by the inline bootstrap in index.html so the saved theme paints on
 * the first frame instead of flashing the default.
 * ═══════════════════════════════════════════════════════════════════════════ */

const SETTINGS_KEY = 'tabi.ui';

const DEFAULTS = {
  theme: 'dark',
  accent: 'green',
  density: 'standard',
  motion: 'full',
  quality: 'balanced',
  speed: 'moderate',
  range: '2h',
};

/** Redraw budget per speed tier, in ms. */
const SPEED_MS = { fast: 250, moderate: 1000, slow: 4000 };

const VALID = {
  theme: ['dark', 'midnight', 'slate', 'light', 'contrast'],
  accent: ['green', 'cyan', 'indigo', 'violet', 'amber', 'rose'],
  density: ['comfortable', 'standard', 'compact'],
  motion: ['full', 'calm', 'off'],
  quality: ['high', 'balanced', 'low'],
  speed: ['fast', 'moderate', 'slow'],
};

function loadSettings() {
  let saved = {};
  try {
    saved = JSON.parse(localStorage.getItem(SETTINGS_KEY) || '{}') || {};
  } catch {
    // Corrupt JSON, or storage blocked in a private window. Defaults are fine;
    // the dashboard must never fail to load over a preference.
  }
  const out = { ...DEFAULTS };
  for (const k of Object.keys(DEFAULTS)) {
    // Validate against the known set rather than trusting storage: a stale value
    // from an older build would otherwise set a data-attribute no CSS matches,
    // leaving the page unstyled.
    if (VALID[k] ? VALID[k].includes(saved[k]) : typeof saved[k] === 'string') {
      out[k] = saved[k];
    }
  }
  return out;
}

const settings = loadSettings();

function saveSettings() {
  try {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings));
  } catch {
    // Quota or private mode: the setting still applies for this session.
  }
}

/** Push the current settings onto <html> and tell the charts to re-read tokens. */
function applySettings() {
  const r = document.documentElement;
  r.dataset.theme = settings.theme;
  r.dataset.accent = settings.accent;
  r.dataset.density = settings.density;
  r.dataset.motion = settings.motion;

  // Keep browser chrome in step, otherwise a light page keeps a dark address bar.
  const cs = document.querySelector('meta[name="color-scheme"]');
  if (cs) cs.content = settings.theme === 'light' ? 'light' : 'dark';
  const tc = document.querySelector('meta[name="theme-color"]');
  if (tc) {
    tc.content = getComputedStyle(r).getPropertyValue('--bg').trim() || '#0f172a';
  }

  // Charts cache their tokens, so the cache must be dropped before they redraw.
  themeChanged();
  for (const c of [reqChart, byteChart]) {
    if (!c) continue;
    c.setQuality(settings.quality);
  }
  recolorCharts();
}

/** True when looping decoration should be skipped entirely. */
function motionOff() {
  return (
    settings.motion === 'off' ||
    window.matchMedia('(prefers-reduced-motion: reduce)').matches
  );
}

/* ── state ──────────────────────────────────────────────────────────────── */

const state = {
  snap: null,
  page: 'overview',
  es: null,
  retry: 0,
  retryTimer: null,
  keyProvider: null,
  keysCache: {},
  modelSort: { col: 'requests', dir: 'desc' },
  lastErrorCount: 0,
  lastOffline: false,
  seenFirst: false,
  /** Per-chart selected range key. */
  range: { req: settings.range, byte: settings.range },
  /** Cached wide-range history: `${res}:${points}` -> { rows, at }. */
  history: {},
  /** Wall-clock ms of the last snapshot, for the freshness pill. */
  lastPush: 0,
  /** Pending render, coalesced by the speed tier. */
  renderTimer: null,
  renderQueued: false,
  /** Previous numeric KPI values, so the ticker can show direction. */
  prevKpi: {},
};

const chime = new Chime();


/* ── charts ─────────────────────────────────────────────────────────────── */

let reqChart = null;
let byteChart = null;

function initCharts() {
  const live = el('div', 'sr-only');
  live.setAttribute('aria-live', 'polite');
  document.body.appendChild(live);

  // Colours come from the live tokens, so a theme or accent change recolours the
  // charts too instead of leaving them on last week's palette.
  const tok = (name, fb) =>
    getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fb;

  reqChart = new LineChart(
    $('#reqChart'),
    [
      { key: 'requests', label: 'Requests', color: tok('--accent', '#22c55e'), fill: true },
      { key: 'errors', label: 'Errors', color: tok('--danger', '#ef4444'), dash: [5, 3] },
      { key: 'avgMs', label: 'Avg ms', color: tok('--info', '#38bdf8'), dash: [2, 3], axis: 'right' },
    ],
    { liveRegion: live, quality: settings.quality, xRes: (RANGES[state.range.req] || {}).res },
  );
  buildLegend($('#reqLegend'), reqChart);

  byteChart = new LineChart(
    $('#byteChart'),
    [
      { key: 'bytesUp', label: 'Uploaded', color: tok('--violet', '#a78bfa'), fmt: fmtBytes },
      { key: 'bytesDown', label: 'Downloaded', color: tok('--accent', '#22c55e'), dash: [5, 3], fmt: fmtBytes, fill: true },
    ],
    { liveRegion: live, quality: settings.quality, xRes: (RANGES[state.range.byte] || {}).res },
  );
  buildLegend($('#byteLegend'), byteChart);
}

/** Re-read series colours from the tokens after a theme/accent change. */
function recolorCharts() {
  const tok = (name, fb) =>
    getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fb;
  const map = {
    requests: '--accent', errors: '--danger', avgMs: '--info',
    bytesUp: '--violet', bytesDown: '--accent',
  };
  for (const c of [reqChart, byteChart]) {
    if (!c) continue;
    for (const s of c.series) {
      if (map[s.key]) s.color = tok(map[s.key], s.color);
    }
    c.draw();
  }
  // The legend swatches carry the same colours, so they have to be rebuilt too.
  if (reqChart) buildLegend($('#reqLegend'), reqChart);
  if (byteChart) buildLegend($('#byteLegend'), byteChart);
}

/** Legends are buttons so series can be toggled by keyboard, not hover. */
function buildLegend(container, chart) {
  container.innerHTML = '';
  for (const s of chart.series) {
    const b = el('button');
    b.type = 'button';
    b.setAttribute('aria-pressed', 'true');
    b.style.color = s.color;
    const sw = el('span', 'swatch' + (s.dash ? ' dashed' : ''));
    sw.style.background = s.dash ? '' : s.color;
    if (s.dash) sw.style.color = s.color;
    b.appendChild(sw);
    b.appendChild(el('span', null, s.label));
    b.addEventListener('click', () => {
      chart.toggle(s.key);
      b.setAttribute('aria-pressed', String(!chart.isHidden(s.key)));
    });
    container.appendChild(b);
  }
}

/* ── tabs (deep-linked via hash so a page is shareable/bookmarkable) ─────── */

function initTabs() {
  $$('nav.tabs button').forEach((btn) => {
    btn.addEventListener('click', () => showPage(btn.dataset.page, true));
  });

  // Left/Right arrows move between tabs (WAI-ARIA tablist pattern).
  $('nav.tabs').addEventListener('keydown', (e) => {
    if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
    const tabs = $$('nav.tabs button');
    const i = tabs.findIndex((t) => t.getAttribute('aria-selected') === 'true');
    const next = e.key === 'ArrowRight' ? (i + 1) % tabs.length : (i - 1 + tabs.length) % tabs.length;
    e.preventDefault();
    tabs[next].focus();
    showPage(tabs[next].dataset.page, true);
  });

  window.addEventListener('hashchange', () => {
    const p = location.hash.replace('#', '');
    if (p) showPage(p, false);
  });

  const initial = location.hash.replace('#', '') || 'overview';
  showPage(initial, false);
}

function showPage(page, pushHash) {
  if (!$('#page-' + page)) page = 'overview';
  state.page = page;

  $$('nav.tabs button').forEach((b) => {
    b.setAttribute('aria-selected', String(b.dataset.page === page));
  });
  $$('main .page').forEach((s) => {
    s.hidden = s.id !== 'page-' + page;
  });

  if (pushHash) history.replaceState(null, '', '#' + page);

  // Only the visible page renders — switching is what triggers its work.
  render();

  // Canvas needs a resize after being unhidden (it had zero size while hidden).
  if (page === 'overview') reqChart?.resize();
  if (page === 'traffic') byteChart?.resize();
  if (page === 'keys' && state.keyProvider) loadKeys(state.keyProvider);
}

/* ── SSE ────────────────────────────────────────────────────────────────── */

function setConn(stateName, text) {
  // Any state other than a healthy stream means the freshness pill should stop
  // claiming "live" — a paused or reconnecting stream is exactly when a frozen
  // dashboard is most misleading.
  if (stateName !== 'live') {
    const pill = $('#fresh');
    if (pill) pill.dataset.stale = stateName === 'paused' ? '1' : '0';
  }
  const c = $('#conn');
  c.dataset.state = stateName;
  $('#connText').textContent = text;
}

function connect() {
  if (state.es) {
    state.es.close();
    state.es = null;
  }
  if (document.hidden) return; // never hold a stream open for a hidden tab

  setConn('connecting', 'Connecting…');
  let es;
  try {
    es = new EventSource('/api/stream');
  } catch {
    scheduleReconnect();
    return;
  }
  state.es = es;

  es.addEventListener('open', () => {
    state.retry = 0;
    setConn('live', 'Live');
  });

  es.addEventListener('snapshot', (e) => {
    let snap;
    try {
      snap = JSON.parse(e.data);
    } catch {
      return; // ignore a malformed frame rather than breaking the page
    }
    onSnapshot(snap);
  });

  es.addEventListener('error', () => {
    // EventSource auto-retries, but we control the cadence and messaging.
    es.close();
    state.es = null;
    if (!navigator.onLine) {
      setConn('offline', 'Browser offline');
    } else {
      setConn('paused', 'Reconnecting…');
    }
    scheduleReconnect();
  });
}

function scheduleReconnect() {
  clearTimeout(state.retryTimer);
  state.retry = Math.min(state.retry + 1, 8);
  const wait = Math.min(1000 * state.retry, 15000);
  state.retryTimer = setTimeout(connect, wait);
}

function onSnapshot(snap) {
  const prev = state.snap;
  state.snap = snap;

  // Alert on transitions only, so the chime cannot spam.
  const errs = snap.totals?.errors ?? 0;
  if (state.seenFirst && errs > state.lastErrorCount) {
    const added = errs - state.lastErrorCount;
    toast('warn', `${added} request error${added > 1 ? 's' : ''} — gateway retried automatically`);
    chime.play(true);
  }
  state.lastErrorCount = errs;

  if (snap.offline !== state.lastOffline) {
    state.lastOffline = snap.offline;
    if (snap.offline) {
      toast('error', 'Network offline — requests held, not failed');
      chime.play(true);
    } else if (state.seenFirst) {
      toast('ok', 'Network restored');
      chime.play(false);
    }
  }

  // Errors tab badge: shows unseen failure count, clears when you visit the tab.
  const errTotal = (snap.errorsLog || []).length;
  const badge = $('#errBadge');
  if (badge) {
    const unseen = Math.max(0, errTotal - (state.seenErrors || 0));
    badge.hidden = unseen === 0 || state.page === 'errors';
    badge.textContent = unseen > 99 ? '99+' : String(unseen);
  }
  if (state.page === 'errors') state.seenErrors = errTotal;

  $('#offlineBanner').hidden = !snap.offline;
  setConn(snap.offline ? 'offline' : 'live', snap.offline ? 'Gateway offline' : 'Live');

  if (!state.keyProvider && snap.providers?.length) {
    state.keyProvider = snap.providers[0].id;
    buildKeyProviderTabs();
  }

  state.seenFirst = true;
  state.lastPush = Date.now();
  paintFreshness();
  scheduleRender();
  if (!prev) {
    $('#brandSub').textContent = `127.0.0.1 · up ${fmtDuration(snap.uptimeSecs)}`;
  }
}

/* Close the stream while hidden. This is the single biggest battery win. */
document.addEventListener('visibilitychange', () => {
  if (document.hidden) {
    if (state.es) {
      state.es.close();
      state.es = null;
    }
    clearTimeout(state.retryTimer);
    // The 1s ticker is pointless behind a hidden tab and browsers throttle it
    // anyway; stopping it explicitly keeps the idle cost at actually zero.
    stopTicker();
    setConn('paused', 'Paused (tab hidden)');
  } else {
    state.retry = 0;
    connect();
    startTicker();
    // Repaint at once: the frozen durations are now badly out of date.
    tickLive();
  }
});

window.addEventListener('online', () => {
  state.retry = 0;
  connect();
});
window.addEventListener('offline', () => setConn('offline', 'Browser offline'));

/* ── render ─────────────────────────────────────────────────────────────── */

/* ══ RENDER SCHEDULING ═════════════════════════════════════════════════════
 * The gateway pushes on every state change, which during a burst is several
 * times a second. Rendering synchronously on each one is what made the UI feel
 * heavy: the DOM work, not the network, was the cost.
 *
 * So pushes update `state.snap` immediately (data is never stale) and the
 * REPAINT is coalesced to the chosen speed tier. Fast = 250ms for a live feel,
 * Slow = 4s for battery. A push that arrives inside the window is not dropped —
 * it repaints at the end of it, with the newest data.
 * ═══════════════════════════════════════════════════════════════════════════ */

function scheduleRender() {
  if (state.renderTimer) {
    state.renderQueued = true;
    return;
  }
  render();
  const budget = SPEED_MS[settings.speed] ?? SPEED_MS.moderate;
  state.renderTimer = setTimeout(() => {
    state.renderTimer = null;
    if (state.renderQueued) {
      state.renderQueued = false;
      scheduleRender();
    }
  }, budget);
}

function render() {
  const s = state.snap;
  if (!s) return;
  switch (state.page) {
    case 'overview': renderOverview(s); break;
    case 'providers': renderProviders(s); break;
    case 'models': renderModels(s); break;
    case 'sessions': renderSessions(s); break;
    case 'keys': renderKeysPage(s); break;
    case 'traffic': renderTraffic(s); break;
    case 'errors': renderErrors(s); break;
    case 'proxies': renderProxies(); break;
    case 'events': renderEvents(s); break;
    case 'settings': renderSettings(); break;
  }
}

/* ══ LIVE TICKER ═══════════════════════════════════════════════════════════
 * The bug this fixes: `elapsedSecs` and `fmtAgo(lastSeen)` are computed by the
 * SERVER at serialisation time. Between pushes they are frozen — and because
 * `inflight_phase` only bumps the revision on a phase CHANGE, a request waiting
 * 130s for a response head produced no pushes for 130s and its timer sat still.
 * The dashboard looked dead while the gateway was working perfectly.
 *
 * So durations are recomputed locally every second from the absolute `started`
 * timestamp the server does send. One interval, text-only writes, no network,
 * and it stops when the tab is hidden.
 * ═══════════════════════════════════════════════════════════════════════════ */

function paintFreshness() {
  const pill = $('#fresh');
  if (!pill) return;
  if (!state.lastPush) {
    $('#freshText').textContent = '—';
    return;
  }
  const age = Math.max(0, Math.round((Date.now() - state.lastPush) / 1000));
  // 45s without a push while connected means something is wrong: the heartbeat
  // alone is 20s, so silence past that is not just "nothing changed".
  const stale = age > 45;
  pill.dataset.stale = stale ? '1' : '0';
  $('#freshText').textContent = age < 2 ? 'live' : `${age}s ago`;
  const bar = pill.querySelector('.bar i');
  if (bar) {
    // Drains over 45s, so the pill shows time passing rather than a static dot.
    bar.style.setProperty('--w', `${Math.max(0, 100 - (age / 45) * 100)}%`);
  }
}

/** Recompute everything that is a function of "now" rather than of the data. */
function tickLive() {
  paintFreshness();

  // In-flight durations. Written straight to the cell so no row is rebuilt —
  // rebuilding would restart the stagger animation once per second.
  const now = Date.now() / 1000;
  for (const cell of $$('[data-started]')) {
    const started = Number(cell.dataset.started);
    if (!started) continue;
    cell.textContent = fmtDuration(Math.max(0, now - started));
  }
  // Relative timestamps ("4m ago") elsewhere in the page.
  for (const cell of $$('[data-ago]')) {
    const at = Number(cell.dataset.ago);
    if (!at) continue;
    cell.textContent = fmtAgo(at);
  }
  // Uptime in the brand line.
  const sub = $('#brandSub');
  if (sub && state.snap && state.lastPush) {
    const drift = Math.round((Date.now() - state.lastPush) / 1000);
    sub.textContent = `127.0.0.1 · up ${fmtDuration(state.snap.uptimeSecs + drift)}`;
  }
}

let liveTimer = null;
function startTicker() {
  if (liveTimer) return;
  liveTimer = setInterval(tickLive, 1000);
}
function stopTicker() {
  clearInterval(liveTimer);
  liveTimer = null;
}

/* ══ NUMBER TICKER ═════════════════════════════════════════════════════════
 * Counts a KPI from its previous value to the new one. Uses rAF and stops at
 * the target, so it is one short burst per change and nothing loops.
 * ═══════════════════════════════════════════════════════════════════════════ */

function tickNumber(node, from, to, fmt) {
  if (motionOff() || from === to || !Number.isFinite(from) || !Number.isFinite(to)) {
    node.textContent = fmt(to);
    return;
  }
  // A huge jump (first paint, or a counter reset) is not worth animating.
  if (from === 0 || Math.abs(to - from) / Math.max(1, Math.abs(from)) > 4) {
    node.textContent = fmt(to);
    return;
  }
  const dur = 420;
  const t0 = performance.now();
  // Cancel any in-flight count on this node, or two animations fight over it.
  if (node._tickRaf) cancelAnimationFrame(node._tickRaf);
  const step = (now) => {
    const p = Math.min(1, (now - t0) / dur);
    // ease-out: fast start, settles into the final value.
    const eased = 1 - Math.pow(1 - p, 3);
    node.textContent = fmt(from + (to - from) * eased);
    if (p < 1) node._tickRaf = requestAnimationFrame(step);
    else {
      node._tickRaf = null;
      node.textContent = fmt(to);
    }
  };
  node._tickRaf = requestAnimationFrame(step);
}

function setKpi(id, value, sub, tone) {
  const v = $('#' + id);
  if (v) v.textContent = value;
  const sb = $('#' + id + 'Sub');
  if (sb) sb.textContent = sub ?? '\u00a0';
  const tile = v?.closest('.kpi');
  if (tile) {
    tile.classList.remove('good', 'warn', 'bad');
    if (tone) tile.classList.add(tone);
  }
}

/**
 * Numeric KPI: counts from the previous value and flashes the direction.
 *
 * Direction is shown by colour AND by the count animating upward/downward, and
 * the number itself is always the source of truth — so nothing here conveys
 * meaning by colour alone (skill: color-not-only).
 */
function setKpiNum(id, num, fmt, sub, tone) {
  const v = $('#' + id);
  if (!v) return;
  const prev = state.prevKpi[id];
  state.prevKpi[id] = num;
  tickNumber(v, prev ?? num, num, (x) => fmt(x));
  if (prev != null && prev !== num) {
    v.classList.remove('up', 'down');
    v.classList.add('tick', num > prev ? 'up' : 'down');
    clearTimeout(v._tickClear);
    v._tickClear = setTimeout(() => v.classList.remove('up', 'down'), 900);
  }
  const sb = $('#' + id + 'Sub');
  if (sb) sb.textContent = sub ?? '\u00a0';
  const tile = v.closest('.kpi');
  if (tile) {
    tile.classList.remove('good', 'warn', 'bad');
    if (tone) tile.classList.add(tone);
  }
}

function renderOverview(s) {
  const t = s.totals;
  const keys = s.providers.reduce((a, p) => a + p.keys, 0);
  const alive = s.providers.reduce((a, p) => a + p.alive, 0);
  const funds = s.providers.reduce((a, p) => a + p.funds, 0);
  const errRate = t.requests ? (t.errors / t.requests) * 100 : 0;

  // Counters tick from their previous value; a jump is visible without a diff.
  setKpiNum('kRequests', t.requests, (n) => fmtInt(Math.round(n)), `up ${fmtDuration(s.uptimeSecs)}`);
  setKpiNum(
    'kErrors', t.errors, (n) => fmtInt(Math.round(n)),
    t.requests ? `${errRate.toFixed(1)}% of requests` : 'none yet',
    t.errors === 0 ? 'good' : errRate > 20 ? 'bad' : 'warn',
  );
  setKpiNum('kCost', t.cost, fmtMoney, 'billed by providers');
  setKpiNum(
    'kKeys', alive, (n) => fmtInt(Math.round(n)),
    `of ${fmtInt(keys)} in pool`,
    alive === 0 ? 'bad' : alive < 10 ? 'warn' : 'good',
  );
  setKpiNum('kFunds', funds, fmtMoney, 'measured, free probes');
  // `sessionsTotal` counts before the snapshot truncates the list, so this stays
  // honest now that only the newest 24 sessions are sent.
  setKpiNum(
    'kSessions', s.activeSessions, (n) => fmtInt(Math.round(n)),
    `${fmtInt(s.sessionsTotal ?? s.sessions.length)} total tracked`,
  );
  setKpiNum('kBytes', t.bytesTotal, fmtBytes, `${fmtBytes(t.bytesUp)} up · ${fmtBytes(t.bytesDown)} down`);
  setKpiNum(
    'kSaves', t.rotations + t.failovers + t.offlineHolds, (n) => fmtInt(Math.round(n)),
    `${t.rotations} key · ${t.failovers} host · ${t.offlineHolds} held`,
    'good',
  );

  // Live ranges are satisfied by the snapshot itself; wider ones are fetched.
  paintChart('req', reqChart);
  renderReqTable(s.minutes);
  renderLive(s);
  renderProviderCards(s);
}

/* ══ CHART RANGE PLUMBING ═══════════════════════════════════════════════════
 * A live range (<=2h of minute buckets) is drawn straight from the snapshot, so
 * it moves with every push at zero network cost. A wider range is fetched from
 * `/api/history`, cached, and refreshed at most once a minute — a 30-day chart
 * does not change between two pushes, and re-pulling it on every push is exactly
 * the kind of work that made the old snapshot heavy.
 * ═══════════════════════════════════════════════════════════════════════════ */

/** Chart-specific series slice from the snapshot's minute buckets. */
function liveRows(s, buckets) {
  const rows = s.minutes || [];
  return buckets >= rows.length ? rows : rows.slice(-buckets);
}

/**
 * Fetch a history range, with caching AND in-flight coalescing.
 *
 * Coalescing matters because two things ask for the same range at once: the
 * range chip's own click handler, and the very next scheduled repaint (which
 * re-runs `paintChart`). Caching alone does not help there — the cache is only
 * written when the response lands, so both callers miss and both fetch. Storing
 * the PROMISE means the second caller awaits the first request instead of
 * issuing a duplicate.
 */
async function fetchHistory(res, points) {
  const cacheKey = `${res}:${points}`;
  const hit = state.history[cacheKey];
  if (hit) {
    // A request already in flight: join it rather than starting another.
    if (hit.promise) return hit.promise;
    if (Date.now() - hit.at < HISTORY_TTL_MS) return hit.rows;
  }

  const promise = (async () => {
    const r = await fetch(`/api/history?res=${encodeURIComponent(res)}&points=${points}`, {
      headers: { accept: 'application/json' },
    });
    if (!r.ok) throw new Error(`history ${r.status}`);
    const j = await r.json();
    return j.rows || [];
  })();

  // Published before the await so a concurrent caller can see it. Keep any
  // previous rows alongside it, so a refresh does not blank an existing chart.
  state.history[cacheKey] = { rows: hit?.rows || [], at: hit?.at || 0, promise };
  try {
    const rows = await promise;
    state.history[cacheKey] = { rows, at: Date.now() };
    return rows;
  } catch (e) {
    // Drop the failed entry entirely so the next attempt is a real retry and
    // not an await on an already-rejected promise.
    delete state.history[cacheKey];
    throw e;
  }
}

/**
 * Draw `chart` for its currently selected range.
 *
 * Never throws: a failed history fetch falls back to the live snapshot rows and
 * says so, because a chart showing 2 hours is far more useful than an error
 * where a chart used to be (skill: error-state-chart).
 */
async function paintChart(which, chart) {
  if (!chart || !state.snap) return;
  const key = state.range[which] || '2h';
  const spec = RANGES[key] || RANGES['2h'];
  const note = $(`#${which}ResNote`);
  const summary = $(`#${which}ChartSummary`);

  chart.setXRes(spec.res);
  if (note) note.textContent = spec.label;

  const finish = (rows, suffix) => {
    chart.setData(rows);
    if (summary) summary.textContent = chart.summary();
    if (note && suffix) note.textContent = `${spec.label} · ${suffix}`;
  };

  if (spec.live) {
    finish(liveRows(state.snap, spec.buckets));
    return;
  }

  // Serve the cache synchronously when it is warm, so switching back to a range
  // you already viewed is instant rather than showing a loading state again.
  const cached = state.history[`${spec.res}:${spec.buckets}`];
  if (cached && !cached.promise && cached.at && Date.now() - cached.at < HISTORY_TTL_MS) {
    finish(cached.rows);
    return;
  }

  chart.setLoading(true);
  try {
    finish(await fetchHistory(spec.res, spec.buckets));
  } catch (e) {
    finish(liveRows(state.snap, 120), 'history unavailable — showing live');
  }
}

/** Wire one range chip group. */
function initRange(which, getChart) {
  const box = $(`#${which}Range`);
  if (!box) return;
  const buttons = Array.from(box.querySelectorAll('button'));
  const paint = () => {
    for (const b of buttons) {
      b.setAttribute('aria-pressed', String(b.dataset.range === state.range[which]));
    }
    paintChart(which, getChart());
  };
  for (const b of buttons) {
    b.addEventListener('click', () => {
      state.range[which] = b.dataset.range;
      // Remember the choice made on the main chart as the default for next load.
      if (which === 'req') {
        settings.range = b.dataset.range;
        saveSettings();
      }
      paint();
    });
  }
  paint();
}

/* ── live in-flight requests ─────────────────────────────────────────────── */

/**
 * Shows requests being served RIGHT NOW.
 *
 * Distinct from the Sessions tab: a session is "active" if seen in the last 45
 * minutes, but "working" means a request is in flight this instant. Without this
 * a session mid-thinking looked identical to one that finished long ago.
 */
function renderLive(s) {
  const tb = $('#liveTable tbody');
  const rows = s.inflight || [];
  $('#liveEmpty').hidden = rows.length > 0;
  setText($('#liveHint'), rows.length
    ? `${rows.length} request${rows.length > 1 ? 's' : ''} in flight`
    : 'a request appears the moment it enters the gateway');

  // Keyed on the gateway's monotonic in-flight id, so a row survives every push
  // for the whole life of its request. Rebuilding here was especially bad: it
  // reset the pulsing phase dot and the elapsed cell that the 1s ticker owns.
  reconcile(
    tb,
    rows,
    (f) => f.id,
    (f) => {
      const tr = el('tr', 'is-live');
      cells(tr, [
        { f: 'label', cls: 'mono' },
        { f: 'client' },
        { f: 'model', cls: 'mono' },
        { f: 'provider' },
        { f: 'key', cls: 'mono' },
        { f: 'phase' },
        { f: 'round', cls: 'num' },
        { f: 'attempts', cls: 'num' },
        { f: 'elapsed', cls: 'num' },
      ]);
      // The phase cell holds the live dot, so it is built once and only its text
      // is patched afterwards — recreating it would restart the pulse.
      const tdph = tr.querySelector('[data-f="phase"]');
      const ph = el('span', 'phase');
      ph.appendChild(el('span', 'live-dot'));
      ph.appendChild(el('span', 'phase-text'));
      tdph.appendChild(ph);
      // data-started hands the elapsed cell to the 1s ticker. The server's
      // elapsedSecs is only a seed: it freezes between pushes, and a request can
      // wait 130s for a response head without producing a single push.
      const tdEl = tr.querySelector('[data-f="elapsed"]');
      if (f.started) tdEl.dataset.started = String(f.started);
      return tr;
    },
    (tr, f) => {
      patchCells(tr, {
        label: f.label || f.session.slice(0, 12),
        client: f.client || 'unknown',
        model: f.model || '—',
        key: f.key || '—',
        round: fmtInt(f.round),
        attempts: fmtInt(f.attempts),
      });
      setText(tr.querySelector('.phase-text'), f.phase || 'starting');

      // Provider badge: only rebuilt when the provider actually changes, which it
      // does on failover.
      const tdp = tr.querySelector('[data-f="provider"]');
      if (tdp && tdp.dataset.p !== (f.provider || '')) {
        tdp.dataset.p = f.provider || '';
        tdp.innerHTML = '';
        if (f.provider) tdp.appendChild(el('span', 'badge ' + f.provider, f.provider));
        else setText(tdp, '—');
      }

      const tdEl = tr.querySelector('[data-f="elapsed"]');
      if (tdEl) {
        if (f.started) tdEl.dataset.started = String(f.started);
        // Seed it now; the ticker keeps it moving between pushes.
        setText(tdEl, fmtDuration(f.elapsedSecs));
      }
    },
  );
}

function renderReqTable(rows) {
  const tb = $('#reqTable tbody');
  // Newest 40 only: a long table is slow to build and nobody scrolls 180 rows.
  // Keyed on the bucket timestamp, so the newest row is inserted and the oldest
  // removed rather than all 40 being rebuilt every push.
  reconcile(
    tb,
    rows.slice(-40).reverse(),
    (r) => r.t,
    () => {
      const tr = el('tr');
      cells(tr, [
        { f: 'time', cls: 'mono' }, { f: 'requests', cls: 'num' },
        { f: 'errors', cls: 'num' }, { f: 'avgMs', cls: 'num' },
      ]);
      return tr;
    },
    (tr, r) => patchCells(tr, {
      time: fmtClock(r.t),
      requests: fmtInt(r.requests),
      errors: fmtInt(r.errors),
      avgMs: fmtInt(r.avgMs),
    }),
  );
}

function renderProviderCards(s) {
  const wrap = $('#providerCards');
  // Which providers are serving something right now — the beam marks them.
  const busy = new Map();
  for (const f of s.inflight || []) {
    if (f.provider) busy.set(f.provider, (busy.get(f.provider) || 0) + 1);
  }

  // Reconciled, not rebuilt: these cards used to replay their entrance animation
  // on every push, which is the flashing every few seconds. Now only a genuinely
  // new provider animates in; the rest just get new numbers written into the
  // nodes that are already on screen.
  reconcile(
    wrap,
    s.providers,
    (p) => p.id,
    (p) => {
      const card = el('div', 'card spot');
      const h = el('h3');
      h.appendChild(el('span', 'badge ' + p.id, p.label));
      h.appendChild(el('span', 'hint', p.host));
      card.appendChild(h);

      const dl = el('dl');
      dl.style.cssText = 'display:grid;grid-template-columns:auto 1fr;gap:6px 14px;margin:0';
      for (const [label, field] of [
        ['Keys usable', 'keys'],
        ['Credit left', 'funds'],
        ['Requests', 'requests'],
        ['Avg latency', 'latency'],
        ['Uptime 24h', 'uptime'],
        ['Hold / request', 'hold'],
      ]) {
        dl.appendChild(el('dt', null, label));
        const dd = el('dd', 'mono');
        dd.dataset.f = field;
        dl.appendChild(dd);
      }
      card.appendChild(dl);

      const status = el('div');
      status.dataset.f = 'status';
      status.style.marginTop = '12px';
      card.appendChild(status);
      return card;
    },
    (card, p) => {
      const set = (field, value) => {
        const n = card.querySelector(`[data-f="${field}"]`);
        if (n) setText(n, value);
      };
      set('keys', `${fmtInt(p.alive)} / ${fmtInt(p.keys)}`);
      set('funds', fmtMoney(p.funds));
      set('requests', `${fmtInt(p.requests)}${p.errors ? ` (${fmtInt(p.errors)} err)` : ''}`);
      set('latency', p.ewmaMs ? `${fmtInt(p.ewmaMs)} ms` : 'not measured');
      set('uptime', `${p.uptimePct}%`);
      set('hold', fmtMoney(p.hold));

      // The beam is decoration; "serving N now" states the same fact in text, so
      // the state is never conveyed by animation alone.
      const n = busy.get(p.id) || 0;
      setClass(card, 'beam', n > 0);

      const status = card.querySelector('[data-f="status"]');
      if (!status) return;
      const label =
        p.consecutiveFails > 2 ? `fail:${p.consecutiveFails}`
        : p.alive === 0 ? 'dry'
        : n > 0 ? `serving:${n}`
        : 'healthy';
      // Badges hold inline SVG, so rebuilding them every push is the expensive
      // part. Only touch them when the state string actually changes.
      if (status.dataset.state === label) return;
      status.dataset.state = label;
      status.innerHTML = '';
      if (p.consecutiveFails > 2) {
        status.appendChild(badgeWith('bad', ICON.warn, `${p.consecutiveFails} failures in a row`));
      } else if (p.alive === 0) {
        status.appendChild(badgeWith('bad', ICON.x, 'no funded keys'));
      } else {
        status.appendChild(badgeWith('ok', ICON.check, 'healthy'));
      }
      if (n > 0) {
        status.appendChild(document.createTextNode(' '));
        status.appendChild(badgeWith('ok', ICON.check, `serving ${n} now`));
      }
    },
  );
}

function badgeWith(kind, icon, text) {
  const b = el('span', 'badge ' + kind);
  b.innerHTML = icon;
  b.appendChild(el('span', null, text));
  b.querySelector('svg').style.cssText = 'width:12px;height:12px';
  return b;
}

function renderProviders(s) {
  // Fetched on a TTL rather than per render. renderProviders runs on every push
  // while the tab is open, and firing an HTTP request per push both wasted work
  // and made the table appear to lag behind the numbers next to it.
  refreshLeaderboard();
  // ── uptime timeline ─────────────────────────────────────────────────────
  //
  // 96 bars x 3 providers = 288 nodes. Rebuilding that on every push was the
  // single most expensive thing the Providers tab did, and it threw away the
  // per-bar tooltips mid-hover.
  const wrap = $('#uptimeRows');
  const SLOTS = 96; // 96 x 5min = 8 hours across the strip

  reconcile(
    wrap,
    s.providers,
    (p) => p.id,
    (p) => {
      const row = el('div', 'uptime-row');
      const head = el('div', 'uptime-head');
      head.dataset.f = 'head';
      row.appendChild(head);
      const bars = el('div', 'uptime-bars');
      bars.setAttribute('role', 'img');
      // Allocate all slots once; only their data-up and title change later.
      for (let i = 0; i < SLOTS; i++) bars.appendChild(el('i'));
      row.appendChild(bars);
      const scale = el('div', 'uptime-scale');
      for (let i = 0; i < 3; i++) scale.appendChild(el('span'));
      row.appendChild(scale);
      return row;
    },
    (row, p) => {
      const tone = p.uptimePct >= 99 ? 'ok' : p.uptimePct >= 90 ? 'warn' : 'bad';
      const head = row.querySelector('[data-f="head"]');
      // The head carries an SVG badge, so rebuild it only when the tone or the
      // percentage actually moves.
      const hstate = `${tone}:${p.uptimePct}`;
      if (head.dataset.s !== hstate) {
        head.dataset.s = hstate;
        head.innerHTML = '';
        head.appendChild(badgeWith(tone, tone === 'ok' ? ICON.check : ICON.warn, p.label));
        head.appendChild(el('span', 'hint', p.host));
        head.appendChild(el('span', 'pct', `${p.uptimePct}% up`));
      }

      const bars = row.querySelector('.uptime-bars');
      const samples = (p.uptime || []).slice(-SLOTS);
      const pad = SLOTS - samples.length;
      let down = 0;
      const kids = bars.children;
      for (let i = 0; i < SLOTS; i++) {
        const b = kids[i];
        if (!b) continue;
        if (i < pad) {
          if (b.dataset.up !== 'x') b.dataset.up = 'x';
          if (b.title !== 'no data') b.title = 'no data';
          continue;
        }
        const smp = samples[i - pad];
        const up = smp.up ? '1' : '0';
        if (!smp.up) down++;
        if (b.dataset.up !== up) b.dataset.up = up;
        // Exact per-bar value on hover AND long-press.
        const title = `${fmtClock(smp.t)} — ${smp.up ? `up, ${smp.ms} ms` : 'unreachable'}`;
        if (b.title !== title) b.title = title;
      }
      bars.setAttribute(
        'aria-label',
        `${p.label} availability: ${samples.length} probes, ${down} failed, ${p.uptimePct} percent up.`,
      );

      const sc = row.querySelector('.uptime-scale').children;
      setText(sc[0], samples.length ? fmtClock(samples[0].t) : 'earlier');
      // Says how many of the retained samples are on screen, so a 120-of-720
      // strip is not mistaken for the whole history.
      setText(sc[1], `${samples.length} of ${fmtInt(p.uptimeSamples ?? samples.length)} probes · free`);
      setText(sc[2], samples.length ? fmtClock(samples[samples.length - 1].t) : 'now');
    },
  );

  // ── latency table ───────────────────────────────────────────────────────
  const tb = $('#latTable tbody');
  const fastest = s.providers
    .filter((p) => p.ewmaMs)
    .sort((a, b) => a.ewmaMs - b.ewmaMs)[0];

  reconcile(
    tb,
    s.providers,
    (p) => p.id,
    () => {
      const tr = el('tr');
      cells(tr, [
        { f: 'name' }, { f: 'host', cls: 'mono' }, { f: 'ms', cls: 'num' },
        { f: 'uptime', cls: 'num' }, { f: 'requests', cls: 'num' },
        { f: 'errors', cls: 'num' }, { f: 'hold', cls: 'num' }, { f: 'state' },
      ]);
      return tr;
    },
    (tr, p) => {
      const name = tr.querySelector('[data-f="name"]');
      if (name.dataset.s !== p.id) {
        name.dataset.s = p.id;
        name.innerHTML = '';
        name.appendChild(el('span', 'badge ' + p.id, p.label));
      }
      patchCells(tr, {
        host: p.host,
        ms: p.ewmaMs ? fmtInt(p.ewmaMs) : '—',
        uptime: `${p.uptimePct}%`,
        requests: fmtInt(p.requests),
        errors: fmtInt(p.errors),
        hold: fmtMoney(p.hold),
      });
      const st = tr.querySelector('[data-f="state"]');
      const label = p.consecutiveFails > 2 ? 'failing'
        : fastest && fastest.id === p.id ? 'preferred'
        : 'standby';
      if (st.dataset.s !== label) {
        st.dataset.s = label;
        st.innerHTML = '';
        if (label === 'failing') st.appendChild(badgeWith('bad', ICON.warn, 'failing'));
        else if (label === 'preferred') st.appendChild(badgeWith('ok', ICON.check, 'preferred'));
        else st.appendChild(badgeWith('', ICON.info, 'standby'));
      }
    },
  );
}

function renderModels(s) {
  const tb = $('#modelTable tbody');
  const rows = [...s.models];
  const { col, dir } = state.modelSort;
  rows.sort((a, b) => {
    const x = a[col], y = b[col];
    const c = typeof x === 'string' ? String(x).localeCompare(String(y)) : (x ?? 0) - (y ?? 0);
    return dir === 'asc' ? c : -c;
  });

  $('#modelEmpty').hidden = rows.length > 0;

  reconcile(
    tb,
    rows,
    (m) => m.id,
    () => {
      const tr = el('tr');
      cells(tr, [
        { f: 'model', cls: 'mono' }, { f: 'requests', cls: 'num' },
        { f: 'errors', cls: 'num' }, { f: 'avgMs', cls: 'num' },
        { f: 'cost', cls: 'num' }, { f: 'in', cls: 'num' },
        { f: 'out', cls: 'num' }, { f: 'providers' },
      ]);
      return tr;
    },
    (tr, m) => {
      patchCells(tr, {
        model: m.id,
        requests: fmtInt(m.requests),
        errors: fmtInt(m.errors),
        avgMs: m.avgMs ? fmtInt(m.avgMs) : '—',
        cost: fmtMoney(m.cost),
        in: fmtInt(m.inTokens),
        out: fmtInt(m.outTokens),
      });
      // Provider badges change rarely; rebuild only when the set does.
      const tdp = tr.querySelector('[data-f="providers"]');
      const key = Object.keys(m.byProvider || {}).sort().join(',');
      if (tdp.dataset.s !== key) {
        tdp.dataset.s = key;
        tdp.innerHTML = '';
        for (const [pid, n] of Object.entries(m.byProvider || {})) {
          tdp.appendChild(el('span', 'badge ' + pid, `${pid} ${fmtInt(n)}`));
          tdp.appendChild(document.createTextNode(' '));
        }
        if (!key) setText(tdp, '—');
      }
    },
  );

  // Advertised-but-unused: models each provider claims to serve that have no
  // traffic yet. Kept in sync with the fast model sync.
  const adv = $('#advertised');
  if (adv) {
    const rows = (s.advertised || []).slice().sort((a, b) => a.id.localeCompare(b.id));
    adv.innerHTML = '';
    if (rows.length === 0) {
      adv.innerHTML = '<div class="empty">No models discovered yet — the fast model sync probes every few seconds.</div>';
    } else {
      rows.forEach((m) => {
        const badge = el('span', 'badge ' + (m.provider || ''), m.id);
        adv.appendChild(badge);
        adv.appendChild(document.createTextNode(' '));
      });
    }
  }

  // Per-provider models: what each provider claims to serve
  const mbp = $('#modelsByProvider');
  if (mbp) {
    mpb.innerHTML = '';
    const providers = (s.providers || []).slice().sort((a, b) => a.id.localeCompare(b.id));
    if (providers.length === 0) {
      mpb.innerHTML = '<div class="empty">No providers configured.</div>';
    } else {
      providers.forEach((p) => {
        const card = el('div', 'card');
        const models = (p.models || []).slice().sort();
        card.innerHTML = `<div class="card-h"><strong>${esc(p.label || p.id)}</strong> <span class="hint">(${p.id})</span></div>
          <div class="sp-models-list">${models.length ? models.map((m) => `<span class="badge mono">${esc(m)}</span>`).join(' ') : '<span class="note">No models discovered yet — auto-probing...</span>'}</div>`;
        mpb.appendChild(card);
      });
    }
  }
}

function initModelSort() {
  $$('#modelTable thead th[data-sort]').forEach((th) => {
    th.style.cursor = 'pointer';
    th.tabIndex = 0;
    const go = () => {
      const col = th.dataset.sort;
      if (state.modelSort.col === col) {
        state.modelSort.dir = state.modelSort.dir === 'asc' ? 'desc' : 'asc';
      } else {
        state.modelSort = { col, dir: 'desc' };
      }
      // aria-sort must reflect the live sort state (WCAG).
      $$('#modelTable thead th[data-sort]').forEach((o) => o.setAttribute('aria-sort', 'none'));
      th.setAttribute('aria-sort', state.modelSort.dir === 'asc' ? 'ascending' : 'descending');
      renderModels(state.snap);
    };
    th.addEventListener('click', go);
    th.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        go();
      }
    });
  });
}

function renderSessions(s) {
  const tb = $('#sessionTable tbody');
  $('#sessionEmpty').hidden = s.sessions.length > 0;

  // Keyed on the session fingerprint. Rebuilding here destroyed the row that a
  // keyboard user had focused, and reset the working-dot animation on every push.
  reconcile(
    tb,
    s.sessions,
    (x) => x.fp,
    (x) => {
      const tr = el('tr', 'clickable');
      // Row opens a detail drawer. tabIndex + Enter/Space so it is keyboard
      // reachable, not mouse-only.
      tr.tabIndex = 0;
      tr.setAttribute('role', 'button');
      tr.setAttribute('aria-label', `Details for session ${x.label || x.fp}`);
      const open = () => openSession(x.fp);
      tr.addEventListener('click', open);
      tr.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open(); }
      });
      cells(tr, [
        { f: 'session' }, { f: 'provider' }, { f: 'key', cls: 'mono' },
        { f: 'turns', cls: 'num' }, { f: 'cost', cls: 'num' },
        { f: 'in', cls: 'num' }, { f: 'out', cls: 'num' },
        { f: 'data', cls: 'num' }, { f: 'models', cls: 'mono' }, { f: 'ago' },
      ]);
      return tr;
    },
    (tr, x) => {
      // Session cell carries an SVG badge and, when working, a pulsing dot.
      // Rebuild only when that state changes, or the pulse restarts every push.
      const td0 = tr.querySelector('[data-f="session"]');
      const sstate = `${x.working ? 'w' : x.active ? 'a' : 'i'}:${x.label || x.fp}`;
      if (td0.dataset.s !== sstate) {
        td0.dataset.s = sstate;
        td0.innerHTML = '';
        td0.appendChild(badgeWith(
          x.working ? 'ok' : '',
          x.working ? ICON.check : ICON.info,
          x.label || x.fp.slice(0, 10),
        ));
        if (x.working) {
          const ph = el('span', 'phase');
          ph.appendChild(el('span', 'live-dot'));
          ph.appendChild(el('span', null, 'working'));
          td0.appendChild(ph);
        }
      }

      const tdp = tr.querySelector('[data-f="provider"]');
      if (tdp.dataset.s !== x.provider) {
        tdp.dataset.s = x.provider;
        tdp.innerHTML = '';
        tdp.appendChild(el('span', 'badge ' + x.provider, x.provider));
      }

      patchCells(tr, {
        key: x.key,
        turns: fmtInt(x.turns),
        cost: fmtMoney(x.cost),
        in: fmtInt(x.inTokens),
        out: fmtInt(x.outTokens),
        data: fmtBytes(x.bytesUp + x.bytesDown),
        models: Object.keys(x.models || {}).join(', ') || '—',
      });

      // data-ago hands this cell to the 1s ticker, so "4m ago" keeps counting
      // even when no push arrives.
      const tdAgo = tr.querySelector('[data-f="ago"]');
      tdAgo.dataset.ago = String(x.lastSeen);
      setText(tdAgo, fmtAgo(x.lastSeen));

      // Working sessions read as live without relying on the dot alone.
      setClass(tr, 'is-live', !!x.working);
    },
  );
}

function renderTraffic(s) {
  const t = s.totals;
  $('#tTotal').textContent = fmtBytes(t.bytesTotal);
  $('#tUp').textContent = fmtBytes(t.bytesUp);
  $('#tDown').textContent = fmtBytes(t.bytesDown);
  $('#tAvg').textContent = t.requests ? fmtBytes(Math.round(t.bytesTotal / t.requests)) : '—';

  paintChart('byte', byteChart);

  const tb = $('#trafficTable tbody');
  const total = s.providers.reduce((a, p) => a + p.bytesUp + p.bytesDown, 0) || 1;

  reconcile(
    tb,
    s.providers,
    (p) => p.id,
    () => {
      const tr = el('tr');
      cells(tr, [
        { f: 'name' }, { f: 'up', cls: 'num' }, { f: 'down', cls: 'num' },
        { f: 'total', cls: 'num' }, { f: 'share', cls: 'num' },
      ]);
      return tr;
    },
    (tr, p) => {
      const name = tr.querySelector('[data-f="name"]');
      if (name.dataset.s !== p.id) {
        name.dataset.s = p.id;
        name.innerHTML = '';
        name.appendChild(el('span', 'badge ' + p.id, p.label));
      }
      const sum = p.bytesUp + p.bytesDown;
      patchCells(tr, {
        up: fmtBytes(p.bytesUp),
        down: fmtBytes(p.bytesDown),
        total: fmtBytes(sum),
        share: `${((sum / total) * 100).toFixed(1)}%`,
      });
    },
  );
}

function renderEvents(s) {
  const ul = $('#eventList');
  $('#eventEmpty').hidden = s.events.length > 0;
  // Keyed on time+message: the same message can legitimately repeat, and the
  // server sends no event id, so the pair is the best available identity.
  reconcile(
    ul,
    s.events,
    (e, i) => `${e.t}:${i}:${e.msg.slice(0, 40)}`,
    (e) => {
      const li = el('li');
      li.dataset.level = e.level;
      li.appendChild(el('time', null, fmtClock(e.t)));
      li.appendChild(el('span', 'lvl', e.level.toUpperCase()));
      li.appendChild(el('span', 'msg', e.msg));
      return li;
    },
    (li, e) => {
      if (li.dataset.level !== e.level) li.dataset.level = e.level;
      setText(li.querySelector('.msg'), e.msg);
    },
  );
}

/* ── keys page ──────────────────────────────────────────────────────────── */

function buildKeyProviderTabs() {
  const wrap = $('#keyProviderTabs');
  wrap.innerHTML = '';
  for (const p of state.snap.providers) {
    const b = el('button', 'btn ghost', p.label);
    b.type = 'button';
    b.setAttribute('aria-pressed', String(p.id === state.keyProvider));
    b.addEventListener('click', () => {
      state.keyProvider = p.id;
      buildKeyProviderTabs();
      loadKeys(p.id);
    });
    wrap.appendChild(b);
  }
}

function renderKeysPage(s) {
  if (!state.keyProvider && s.providers.length) state.keyProvider = s.providers[0].id;
  if (!$('#keyProviderTabs').children.length) buildKeyProviderTabs();
  const cached = state.keysCache[state.keyProvider];
  if (cached) paintKeys(cached);
  else loadKeys(state.keyProvider);
}

async function loadKeys(provider) {
  if (!provider) return;
  try {
    const r = await fetch(`/api/keys?provider=${encodeURIComponent(provider)}&limit=200`);
    if (!r.ok) throw new Error('HTTP ' + r.status);
    const data = await r.json();
    state.keysCache[provider] = data;
    if (state.page === 'keys') paintKeys(data);
  } catch (e) {
    $('#keyTableNote').textContent = `Could not load keys: ${e.message}`;
  }
}

function paintKeys(data) {
  const tb = $('#keyTable tbody');
  reconcile(
    tb,
    data.rows,
    (k) => k.key,
    () => {
      const tr = el('tr');
      cells(tr, [
        { f: 'key', cls: 'mono' }, { f: 'balance', cls: 'num' },
        { f: 'spent', cls: 'num' }, { f: 'turns', cls: 'num' },
        { f: 'source' }, { f: 'state' },
      ]);
      return tr;
    },
    (tr, k) => {
      patchCells(tr, {
        key: k.key,
        balance: k.balance == null ? 'unknown' : fmtMoney(k.balance),
        spent: fmtMoney(k.spent),
        turns: fmtInt(k.turns),
        source: k.exact ? 'exact' : k.balance == null ? 'unprobed' : 'estimated',
      });
      const td = tr.querySelector('[data-f="state"]');
      const label = k.cooling ? `cool:${k.reason}:${k.coolingFor}`
        : k.usable ? 'ready' : 'insufficient';
      if (td.dataset.s === label) return;
      td.dataset.s = label;
      td.innerHTML = '';
      if (k.cooling) {
        td.appendChild(badgeWith('warn', ICON.warn, `${k.reason} · ${fmtDuration(k.coolingFor)}`));
      } else if (k.usable) {
        td.appendChild(badgeWith('ok', ICON.check, 'ready'));
      } else {
        td.appendChild(badgeWith('bad', ICON.x, 'insufficient'));
      }
    },
  );
  $('#keyTableNote').textContent =
    `Showing ${data.rows.length} of ${fmtInt(data.total)} keys, highest balance first. ` +
    `Balances come from free billing probes; "exact" means a provider reported the precise remainder.`;
}

function initKeyForm() {
  const form = $('#addKeyForm');
  const input = $('#keyInput');
  const btn = $('#addBtn');
  const label = $('#addBtnLabel');
  const result = $('#keyResult');
  const reveal = $('#revealBtn');

  reveal.addEventListener('click', () => {
    const showing = input.type === 'text';
    input.type = showing ? 'password' : 'text';
    reveal.setAttribute('aria-pressed', String(!showing));
    reveal.textContent = showing ? 'Show' : 'Hide';
    reveal.setAttribute('aria-label', showing ? 'Show key characters' : 'Hide key characters');
  });

  // Validate on blur, not on keystroke (skill: inline-validation).
  input.addEventListener('blur', () => {
    const v = input.value.trim();
    if (v && !v.startsWith('sk-')) {
      showResult('bad', 'That does not look like an API key', [
        ['Expected', 'a value beginning with sk-'],
        ['Got', v.slice(0, 12) + '…'],
      ]);
    }
  });

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const key = input.value.trim();

    if (!key) {
      showResult('bad', 'Enter a key first', [['Fix', 'paste a key beginning with sk-']]);
      input.focus(); // focus the invalid field (skill: focus-management)
      return;
    }
    if (!key.startsWith('sk-') || key.length < 20) {
      showResult('bad', 'Not a valid key format', [
        ['Cause', 'keys start with sk- and are at least 20 characters'],
        ['Fix', 'check for a truncated copy/paste'],
      ]);
      input.focus();
      return;
    }

    btn.disabled = true;
    label.textContent = 'Checking…';
    const spin = el('span', 'spin');
    btn.prepend(spin);
    showResult('', 'Verifying against every provider (free endpoints only)…', []);

    try {
      const r = await fetch('/api/keys/verify', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ key }),
      });
      const data = await r.json();

      if (data.ok) {
        showResult('ok', data.message || 'Key added', [
          ['Provider', data.providerLabel || data.provider],
          ['Balance', data.balance != null ? fmtMoney(data.balance) : 'unknown'],
          ['Spent', data.usageCents != null ? fmtMoney(data.usageCents / 100) : 'unknown'],
          ['Models', (data.models || []).join(', ') || '—'],
        ]);
        toast('ok', `Key added to ${data.providerLabel || data.provider}`);
        chime.play(false);
        input.value = '';
        loadKeys(state.keyProvider);
      } else {
        const rows = [['Reason', data.error || 'rejected']];
        if (data.provider) rows.push(['Provider', data.provider]);
        if (data.balance != null) rows.push(['Balance', fmtMoney(data.balance)]);
        if (data.duplicate) rows.push(['Fix', 'this key is already in the pool — nothing to do']);
        else rows.push(['Fix', 'check the key is correct and still has credit']);
        (data.tried || []).forEach((t) => rows.push([t.provider || 'tried', t.result || '']));
        showResult('bad', 'Key not added', rows);
        toast('warn', 'Key not added — see details');
      }
    } catch (err) {
      showResult('bad', 'Verification request failed', [
        ['Cause', err.message],
        ['Fix', 'is the gateway still running? Try again.'],
      ]);
    } finally {
      spin.remove();
      btn.disabled = false;
      label.textContent = 'Verify & add';
    }
  });

  function showResult(kind, title, rows) {
    result.hidden = false;
    result.dataset.kind = kind;
    result.innerHTML = '';
    const t = el('div', 'title');
    if (kind === 'ok') t.innerHTML = ICON.check;
    else if (kind === 'bad') t.innerHTML = ICON.x;
    else t.innerHTML = ICON.info;
    const svg = t.querySelector('svg');
    if (svg) svg.style.cssText = 'width:16px;height:16px';
    t.appendChild(el('span', null, title));
    result.appendChild(t);
    if (rows?.length) {
      const dl = el('dl');
      for (const [k, v] of rows) {
        dl.appendChild(el('dt', null, k));
        dl.appendChild(el('dd', null, String(v)));
      }
      result.appendChild(dl);
    }
  }
}


/* ── errors tab ──────────────────────────────────────────────────────────── */

// A failure here does not mean the user's request failed: the gateway retries.
// The colour/severity reflects "did this cost us a key" rather than "was this
// bad", which is why WAF and UPSTREAM are warnings, not errors.
const ERR_TONE = {
  QUOTA: 'bad', AUTH: 'bad',
  RATE: 'warn', WAF: 'warn', UPSTREAM: 'warn', TRANSPORT: 'warn',
  OFFLINE: '', CLIENT: '',
};

let errFilter = '';

function renderErrors(s) {
  const byClass = s.errorsByClass || {};
  const all = s.errorsLog || [];

  // ── breakdown tiles ──────────────────────────────────────────────────────
  const kpis = $('#errClassKpis');
  kpis.innerHTML = '';
  const total = Object.values(byClass).reduce((a, b) => a + b, 0);
  if (!total) {
    const k = el('div', 'kpi good');
    k.appendChild(el('span', 'kpi-label', 'Failures'));
    k.appendChild(el('div', 'kpi-value num', '0'));
    k.appendChild(el('div', 'kpi-sub', 'nothing has gone wrong'));
    kpis.appendChild(k);
  } else {
    for (const [cls, n] of Object.entries(byClass).sort((a, b) => b[1] - a[1])) {
      const k = el('div', 'kpi ' + (ERR_TONE[cls] || ''));
      k.appendChild(el('span', 'kpi-label', cls));
      k.appendChild(el('div', 'kpi-value num', fmtInt(n)));
      k.appendChild(el('div', 'kpi-sub', ERR_HINT[cls] || 'see reference below'));
      kpis.appendChild(k);
    }
  }

  // ── filter chips ─────────────────────────────────────────────────────────
  const fw = $('#errFilter');
  fw.innerHTML = '';
  const mk = (label, value) => {
    const b = el('button', 'btn ghost', label);
    b.type = 'button';
    b.setAttribute('aria-pressed', String(errFilter === value));
    b.addEventListener('click', () => {
      errFilter = value;
      renderErrors(state.snap);
    });
    fw.appendChild(b);
  };
  mk(`All (${fmtInt(all.length)})`, '');
  for (const cls of Object.keys(byClass).sort()) mk(`${cls} (${byClass[cls]})`, cls);

  // ── table ────────────────────────────────────────────────────────────────
  const rows = errFilter ? all.filter((e) => e.class === errFilter) : all;
  const tb = $('#errTable tbody');
  $('#errEmpty').hidden = rows.length > 0;

  // Keyed on the fields that identify one failure. Rebuilding threw away text
  // selection mid-copy, which is the main thing anyone does on this table.
  reconcile(
    tb,
    rows,
    (e) => `${e.t}:${e.key}:${e.round}:${e.status}`,
    () => {
      const tr = el('tr');
      cells(tr, [
        { f: 'time', cls: 'mono' }, { f: 'class' }, { f: 'provider' },
        { f: 'key', cls: 'mono' }, { f: 'status', cls: 'num' },
        { f: 'what' }, { f: 'action' }, { f: 'round', cls: 'num' },
        { f: 'took', cls: 'num' }, { f: 'body', cls: 'num' },
        { f: 'budget', cls: 'num' }, { f: 'egress', cls: 'mono' },
      ]);
      return tr;
    },
    (tr, e) => {
      const tdc = tr.querySelector('[data-f="class"]');
      if (tdc.dataset.s !== e.class) {
        tdc.dataset.s = e.class;
        tdc.innerHTML = '';
        tdc.appendChild(el('span', 'badge ' + (ERR_TONE[e.class] || ''), e.class));
      }
      const tdp = tr.querySelector('[data-f="provider"]');
      if (tdp.dataset.s !== e.provider) {
        tdp.dataset.s = e.provider;
        tdp.innerHTML = '';
        tdp.appendChild(el('span', 'badge ' + e.provider, e.provider));
      }

      patchCells(tr, {
        time: fmtClock(e.t),
        key: e.key || '—',
        status: e.status || 'transport',
        what: explainError(e),
        action: e.action,
        round: fmtInt(e.round),
        took: e.latencyMs ? fmtDuration(Math.round(e.latencyMs / 1000)) : '—',
        // Request size against the budget in force. This pairing is the whole
        // diagnosis for a size-related timeout — a big body against a small
        // budget cannot succeed no matter which key is used.
        body: e.reqBytes ? fmtBytes(e.reqBytes) : '—',
        budget: e.budgetSecs ? `${e.budgetSecs}s` : '—',
        egress: e.proxy || 'direct',
      });

      const tdb = tr.querySelector('[data-f="budget"]');
      setClass(tdb, 'bad', !!e.headTimeout);
      tdb.title = e.headTimeout ? 'died waiting for the response head on this budget' : '';
    },
  );
}

const ERR_HINT = {
  QUOTA: 'key out of funds — rotated',
  AUTH: 'key invalid — retired',
  RATE: '429 — key healthy, cooled',
  WAF: 'Cloudflare block — our IP, not the key',
  UPSTREAM: 'provider 5xx — failed over',
  TRANSPORT: 'connection failed — failed over',
  OFFLINE: 'no network — request held',
  CLIENT: 'bad request — passed through',
};

/**
 * Turn an upstream message into something readable.
 *
 * Cloudflare interstitials are multi-kilobyte HTML documents; showing the raw
 * text would make the table unusable, so the actual reason is extracted.
 */
function explainError(e) {
  const m = e.message || '';
  if (/<!doctype|<html/i.test(m)) {
    const low = m.toLowerCase();
    if (e.status === 524 || low.includes('timed out')) {
      return 'Cloudflare 524 — origin took too long (large request or slow model)';
    }
    if (e.status === 502 || low.includes('bad gateway')) return 'Cloudflare 502 — bad gateway';
    if (low.includes('attention required')) return 'Cloudflare challenge page';
    return `Cloudflare HTML error page (${e.status})`;
  }
  if (e.remaining != null) {
    const need = e.required != null ? ` (needed ${fmtMoney(e.required)})` : '';
    return e.remaining < 0
      ? `account overdrawn ${fmtMoney(e.remaining)}`
      : `balance ${fmtMoney(e.remaining)}${need}`;
  }
  // Prefer the provider's own message field over the raw JSON envelope.
  try {
    const j = JSON.parse(m);
    const msg = j?.error?.message || j?.message;
    if (msg) return String(msg).slice(0, 130);
  } catch { /* not json, fall through */ }
  return m.slice(0, 130) || '—';
}

/* ── egress / proxies tab ────────────────────────────────────────────────── */

async function renderProxies() {
  let d;
  try {
    const r = await fetch('/api/proxies');
    if (!r.ok) throw new Error('HTTP ' + r.status);
    d = await r.json();
  } catch (err) {
    $('#pxMode').textContent = 'error';
    $('#pxModeSub').textContent = err.message;
    return;
  }

  $('#pxMode').textContent = d.enabled ? 'Rotating' : 'Direct';
  $('#pxMode').closest('.kpi').classList.toggle('good', d.enabled);
  $('#pxMode').closest('.kpi').classList.toggle('warn', !d.enabled);
  $('#pxModeSub').textContent = d.enabled
    ? 'provider sees proxy IPs'
    : 'provider sees this device IP';
  $('#pxCount').textContent = fmtInt(d.count);
  $('#pxCountSub').textContent = d.cooling ? `${d.cooling} cooling down` : 'all healthy';
  $('#pxRot').textContent = fmtInt(d.rotations);
  $('#pxDirect').textContent = fmtInt(d.directFallbacks);

  const tb = $('#pxTable tbody');
  reconcile(
    tb,
    d.rows,
    (r) => r.addr,
    () => {
      const tr = el('tr');
      cells(tr, [
        { f: 'addr', cls: 'mono' }, { f: 'auth' }, { f: 'ok', cls: 'num' },
        { f: 'fail', cls: 'num' }, { f: 'ms', cls: 'num' },
        { f: 'state' }, { f: 'err' },
      ]);
      return tr;
    },
    (tr, r) => {
      patchCells(tr, {
        addr: r.addr,
        auth: r.auth ? 'yes' : 'no',
        ok: fmtInt(r.ok),
        fail: fmtInt(r.fail),
        ms: r.ewmaMs ? `${fmtInt(r.ewmaMs)} ms` : '—',
        err: r.lastError ? r.lastError.slice(0, 60) : '—',
      });
      const tds = tr.querySelector('[data-f="state"]');
      const label = r.cooling ? `cool:${r.coolingFor}`
        : r.fail > 0 && r.ok === 0 ? 'failing' : 'ready';
      if (tds.dataset.s === label) return;
      tds.dataset.s = label;
      tds.innerHTML = '';
      if (r.cooling) {
        tds.appendChild(badgeWith('warn', ICON.warn, `cooling ${fmtDuration(r.coolingFor)}`));
      } else if (r.fail > 0 && r.ok === 0) {
        tds.appendChild(badgeWith('bad', ICON.x, 'failing'));
      } else {
        tds.appendChild(badgeWith('ok', ICON.check, 'ready'));
      }
    },
  );
}

/* ══ SETTINGS ── provider + routing editor, posts the whole Settings doc back */

let settingsDraft = null; // working copy; null until first fetch

async function renderSettings() {
  let cur;
  try {
    // Refetch only when no draft is being edited, so typing is never clobbered
    // by a background SSE-driven re-render.
    if (!settingsDraft) {
      const r = await fetch('/api/settings');
      if (!r.ok) throw new Error('HTTP ' + r.status);
      settingsDraft = await r.json();
    }
    cur = settingsDraft;
  } catch (err) {
    $('#settingsStatus').textContent = 'load failed: ' + err.message;
    return;
  }

  // ── providers ──
  const cards = $('#settingsProviderCards');
  cards.innerHTML = '';
  cur.providers.forEach((p, i) => {
    const card = el('div', 'card');
    card.innerHTML = `
      <div class="card-h"><strong>${esc(p.label || p.id)}</strong>
        <button type="button" class="btn ghost sp-del" data-i="${i}">Remove</button>
      </div>
      <div class="sp-grid">
        <label>id <input class="sp-in" data-k="id" value="${esc(p.id)}"></label>
        <label>label <input class="sp-in" data-k="label" value="${esc(p.label)}"></label>
        <label>keys file <input class="sp-in" data-k="keys_file" value="${esc(p.keys_file)}"></label>
        <label>hold $ <input class="sp-in" type="number" step="0.01" data-k="hold" value="${p.hold}"></label>
        <label>initial guess $ <input class="sp-in" type="number" step="0.01" data-k="initial_guess" value="${p.initial_guess}"></label>
        <label>bias <input class="sp-in" type="number" step="0.1" data-k="bias" value="${p.bias || 0}"></label>
        <label>note <input class="sp-in" data-k="note" value="${esc(p.note || '')}"></label>
        <label class="sp-check"><input type="checkbox" data-k="enabled" ${p.enabled ? 'checked' : ''}> enabled</label>
      </div>
      <div class="sp-models">
        <label>models (comma-separated, auto-filled from /v1/models)
          <input class="sp-in" data-k="models" value="${esc((p.models || []).join(', '))}" placeholder="claude-opus-5, claude-sonnet-4-20250514">
        </label>
        <label>model map (client → upstream, comma-separated key=value)
          <input class="sp-in" data-k="model_map" value="${esc(Object.entries(p.model_map || {}).map(([k,v]) => `${k}=${v}`).join(', '))}" placeholder="claude-opus-5=tabi/claude-opus-5">
        </label>
      </div>
      <div class="sp-hosts">${(p.hosts || []).map((h, j) => `
        <div class="sp-host">
          <input class="sp-in sp-hostname" data-h="${j}" value="${esc(h.host)}" placeholder="host">
          <input class="sp-in sp-hostnote" data-h="${j}" value="${esc(h.note || '')}" placeholder="note">
          <label class="sp-check"><input type="checkbox" class="sp-hoston" data-h="${j}" ${h.enabled ? 'checked' : ''}> on</label>
          <button type="button" class="btn ghost sp-hostdel" data-h="${j}">×</button>
        </div>`).join('')}
        <button type="button" class="btn ghost sp-hostadd">+ host</button>
      </div>`;

    card.addEventListener('input', (e) => {
      const t = e.target;
      const q = settingsDraft.providers[i];
      if (t.classList.contains('sp-hostname')) q.hosts[+t.dataset.h].host = t.value;
      else if (t.classList.contains('sp-hostnote')) q.hosts[+t.dataset.h].note = t.value;
      else if (t.classList.contains('sp-hoston')) q.hosts[+t.dataset.h].enabled = t.checked;
      else if (t.dataset.k === 'enabled') q.enabled = t.checked;
      else if (t.dataset.k === 'hold' || t.dataset.k === 'initial_guess' || t.dataset.k === 'bias') q[t.dataset.k] = +t.value || 0;
      else if (t.dataset.k === 'models') {
        q.models = t.value.split(',').map((s) => s.trim()).filter(Boolean);
      } else if (t.dataset.k === 'model_map') {
        q.model_map = {};
        t.value.split(',').forEach((pair) => {
          const [k, ...rest] = pair.split('=');
          if (k && rest.length) q.model_map[k.trim()] = rest.join('=').trim();
        });
      } else if (t.dataset.k) q[t.dataset.k] = t.value;
    });
    card.addEventListener('click', (e) => {
      const t = e.target;
      if (t.classList.contains('sp-del')) {
        settingsDraft.providers.splice(i, 1);
        renderSettings();
      } else if (t.classList.contains('sp-hostadd')) {
        settingsDraft.providers[i].hosts.push({ host: '', enabled: true, note: '' });
        renderSettings();
      } else if (t.classList.contains('sp-hostdel')) {
        settingsDraft.providers[i].hosts.splice(+t.dataset.h, 1);
        renderSettings();
      }
    });
    cards.appendChild(card);
  });

  // ── routing ──
  const R = cur.routing;
  const fields = [
    ['slow_multiplier', 'Slow multiplier', 'P50 TTFB × this = "slow"'],
    ['min_samples', 'Min samples', 'samples before latency is trusted'],
    ['error_weight', 'Error weight', 'errors dominate routing score'],
    ['streak_weight', 'Streak weight', 'consecutive-fail weight (squared)'],
    ['streak_halflife_secs', 'Streak halflife (s)', 'streak decays over this'],
    ['breaker_trip', 'Breaker trip', 'consecutive fails → circuit opens'],
    ['breaker_backoff_secs', 'Breaker backoff (s)', 'comma-separated, per streak depth'],
    ['missing_model_penalty', 'Missing-model penalty', 'score penalty for unadvertised model'],
    ['sticky_escape_multiplier', 'Sticky escape ×', 'abandon sticky if this much slower'],
  ];
  $('#settingsRouting').innerHTML = '<div class="sp-grid">' + fields.map(([k, label, hint]) => `
    <label class="sp-lab">${label}
      <input class="sp-in sp-routing" data-k="${k}"
        value="${k === 'breaker_backoff_secs' ? (R[k] || []).join(',') : R[k]}">
      <span class="note">${hint}</span>
    </label>`).join('') + '</div>'
    + `<div class="sp-checks" style="margin-top:var(--s2)">
      <label class="sp-check"><input type="checkbox" class="sp-routing-bool" data-k="probe_heals_score" ${R.probe_heals_score ? 'checked' : ''}> Uptime probe heals failure streak</label>
      <label class="sp-check"><input type="checkbox" class="sp-routing-bool" data-k="session_stickiness" ${R.session_stickiness ? 'checked' : ''}> Session stickiness</label>
    </div>`;
}

function initSettingsPage() {
  // Delegated once — renderSettings rebuilds the inputs on every render, so
  // binding per element would leak a listener per render.
  $('#settingsRouting')?.addEventListener('input', (e) => {
    const t = e.target, k = t.dataset?.k;
    if (!k || !settingsDraft) return;
    if (k === 'breaker_backoff_secs') {
      settingsDraft.routing[k] = t.value.split(',').map((x) => +x.trim()).filter((x) => x > 0);
    } else if (t.type === 'checkbox') {
      settingsDraft.routing[k] = t.checked;
    } else {
      settingsDraft.routing[k] = +t.value || 0;
    }
  });

  $('#addProviderBtn')?.addEventListener('click', () => {
    if (!settingsDraft) return;
    settingsDraft.providers.push({
      id: 'new-provider',
      label: 'New provider',
      hosts: [{ host: '', enabled: true, note: '' }],
      keys_file: '',
      hold: 0.10,
      initial_guess: 50.0,
      enabled: false,
      bias: 0,
      note: '',
    });
    renderSettings();
  });

  $('#saveSettingsBtn')?.addEventListener('click', async () => {
    if (!settingsDraft) return;
    const status = $('#settingsStatus');
    status.textContent = 'saving…';
    try {
      const r = await fetch('/api/settings', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(settingsDraft),
      });
      const d = await r.json();
      if (!r.ok || d.ok === false) throw new Error(d.message || d.error || ('HTTP ' + r.status));
      status.textContent = 'saved — hot-reloaded, no restart needed';
      toast('ok', 'Settings saved');
    } catch (e) {
      status.textContent = 'save failed: ' + e.message;
      toast('warn', 'Settings save failed: ' + e.message);
    }
  });
}

function initProxyControls() {
  $('#pxToggle')?.addEventListener('click', async () => {
    try {
      const r = await fetch('/api/proxies/toggle', { method: 'POST' });
      const d = await r.json();
      toast('ok', `Proxies ${d.enabled ? 'enabled' : 'disabled'}`);
      renderProxies();
    } catch (e) {
      toast('warn', 'Toggle failed: ' + e.message);
    }
  });
  $('#pxReload')?.addEventListener('click', async () => {
    try {
      const r = await fetch('/api/proxies/reload', { method: 'POST' });
      const d = await r.json();
      // A refused file must not report success. The old handler said
      // "Reloaded 0 proxies" in a green toast while the pool emptied itself.
      if (d.keptExisting) {
        toast('warn', d.message || 'File unreadable — kept the current pool');
      } else if (d.skipped) {
        toast('warn', `Loaded ${d.count}, skipped ${d.skipped} unparseable line${d.skipped > 1 ? 's' : ''}`);
      } else {
        toast('ok', `Reloaded ${d.count} proxies`);
      }
      renderProxies();
    } catch (e) {
      toast('warn', 'Reload failed: ' + e.message);
    }
  });
  $('#pxMaintain')?.addEventListener('click', async () => {
    try {
      const r = await fetch('/api/proxies/maintain', { method: 'POST' });
      const d = await r.json();
      toast('ok', d.message || 'Maintenance started');
      // The cycle vets candidates over several seconds, so refresh once it has
      // had time to finish rather than immediately showing the old numbers.
      setTimeout(renderProxies, 6000);
    } catch (e) {
      toast('warn', 'Maintenance failed: ' + e.message);
    }
  });
}

/* ── provider leaderboard ────────────────────────────────────────────────── */

/**
 * Leaderboard, fetched on a TTL and reconciled in place.
 *
 * It lives behind its own endpoint because the score breakdown is derived from
 * routing internals (decayed streaks, effective error rates) that the snapshot
 * does not carry — putting it in the snapshot would mean recomputing it on every
 * state change, for a table that is only visible on one tab.
 *
 * TTL, not per-render: this is called from renderProviders, which runs on every
 * push. One fetch per push meant ~1 req/s of self-inflicted load, and the async
 * result could land out of order with the synchronous numbers beside it.
 */
const LB_TTL_MS = 3000;
let lbAt = 0;
let lbInFlight = null;

function refreshLeaderboard(force = false) {
  if (!force && Date.now() - lbAt < LB_TTL_MS) return lbInFlight;
  // Coalesce: a second caller during the request joins it instead of starting
  // another (the same bug the history fetch had).
  if (lbInFlight) return lbInFlight;
  lbInFlight = (async () => {
    try {
      const r = await fetch('/api/leaderboard', { headers: { accept: 'application/json' } });
      if (!r.ok) throw new Error(`leaderboard ${r.status}`);
      const d = await r.json();
      lbAt = Date.now();
      paintLeaderboard(d);
    } catch {
      // Leave the previous table on screen. A stale ranking is more useful than
      // an empty one, and the numbers beside it say when data last arrived.
    } finally {
      lbInFlight = null;
    }
  })();
  return lbInFlight;
}

function paintLeaderboard(d) {
  const tb = $('#lbTable tbody');
  if (!tb) return;
  const rows = d.rows || [];

  reconcile(
    tb,
    rows,
    (p) => p.id,
    () => {
      const tr = el('tr');
      cells(tr, [
        { f: 'rank' }, { f: 'provider' }, { f: 'score', cls: 'num' }, { f: 'bars' },
        { f: 'latency', cls: 'num' }, { f: 'success', cls: 'num' }, { f: 'uptime', cls: 'num' },
        { f: 'alive', cls: 'num' }, { f: 'funds', cls: 'num' }, { f: 'cpr', cls: 'num' },
      ]);
      // Structural children built once; only their widths/text change later.
      const tdr = tr.querySelector('[data-f="rank"]');
      tdr.appendChild(el('span', 'rank'));
      const tdb = tr.querySelector('[data-f="bars"]');
      tdb.appendChild(el('div', 'stack'));
      tdb.appendChild(el('div', 'stack-legend'));
      return tr;
    },
    (tr, p) => {
      const rk = tr.querySelector('.rank');
      setText(rk, String(p.rank));
      setClass(rk, 'first', p.rank === 1);

      const tdp = tr.querySelector('[data-f="provider"]');
      const pstate = `${p.id}:${p.breakerOpen ? 1 : 0}`;
      if (tdp.dataset.s !== pstate) {
        tdp.dataset.s = pstate;
        tdp.innerHTML = '';
        tdp.appendChild(el('span', 'badge ' + p.id, p.label));
        if (p.breakerOpen) tdp.appendChild(badgeWith('bad', ICON.warn, 'circuit open'));
      }

      patchCells(tr, {
        score: p.score.toFixed(2),
        latency: p.latencyMs ? `${fmtInt(p.latencyMs)} ms` : '—',
        success: `${p.successRate}%`,
        uptime: `${p.uptimePct}%`,
        alive: fmtInt(p.aliveKeys),
        funds: fmtMoney(p.funds),
        cpr: p.costPerRequest ? fmtMoney(p.costPerRequest) : '—',
      });

      // Stacked breakdown of WHY the score is what it is. Segments are keyed by
      // class so widths are updated rather than the bar being rebuilt.
      const c = p.components || {};
      const sum = Math.max(p.score, 0.001);
      const stack = tr.querySelector('.stack');
      for (const [k, cls] of [
        ['latency', 'lat'], ['errorRate', 'err'], ['streak', 'strk'],
        ['breaker', 'brk'], ['dry', 'dry'],
      ]) {
        const v = c[k] || 0;
        let seg = stack.querySelector('i.' + cls);
        if (v <= 0) {
          if (seg) seg.remove();
          continue;
        }
        if (!seg) {
          seg = el('i', cls);
          stack.appendChild(seg);
        }
        const w = `${Math.min(100, (v / sum) * 100)}%`;
        if (seg.style.width !== w) seg.style.width = w;
        const title = `${k}: ${v.toFixed(2)}`;
        if (seg.title !== title) seg.title = title;
      }
      setText(
        tr.querySelector('.stack-legend'),
        `lat ${(c.latency || 0).toFixed(1)} · err ${(c.errorRate || 0).toFixed(1)} · streak ${(c.streak || 0).toFixed(1)}`,
      );
    },
  );

  const w = d.weights || {};
  setText(
    $('#lbWeights'),
    `Score = latency(s) + errorRate x${w.errorWeight} + streak² x${w.streakWeight} ` +
      `+ penalties. Failure streaks halve every ${w.streakHalflifeSecs}s, so a provider ` +
      `recovers on its own. Lower is better.`,
  );
}

/* ── session detail drawer ───────────────────────────────────────────────── */

let sdLastFocus = null;

async function openSession(fp) {
  sdLastFocus = document.activeElement;
  const drawer = $('#sdDrawer');
  const backdrop = $('#sdBackdrop');
  drawer.hidden = false;
  backdrop.hidden = false;
  $('#sdBody').innerHTML = '<div class="empty">Loading…</div>';
  $('#sdClose').focus(); // move focus into the dialog (WCAG)

  let d;
  try {
    const r = await fetch('/api/session?fp=' + encodeURIComponent(fp));
    d = await r.json();
  } catch (e) {
    $('#sdBody').innerHTML = '';
    $('#sdBody').appendChild(el('div', 'empty', 'Could not load: ' + e.message));
    return;
  }
  if (d.error) {
    $('#sdBody').innerHTML = '';
    $('#sdBody').appendChild(el('div', 'empty', d.error));
    return;
  }

  $('#sdTitle').textContent = d.label || d.fp.slice(0, 16);
  const body = $('#sdBody');
  body.innerHTML = '';

  const status = el('div', 'row');
  status.style.marginBottom = '16px';
  if (d.working) status.appendChild(badgeWith('ok', ICON.check, 'working now'));
  else if (d.active) status.appendChild(badgeWith('', ICON.info, 'active'));
  else status.appendChild(badgeWith('', ICON.info, `idle ${fmtDuration(d.idleSecs)}`));
  status.appendChild(el('span', 'badge ' + d.provider, d.provider));
  if (d.clientLabel) status.appendChild(el('span', 'badge', d.clientLabel));
  if (d.api) status.appendChild(el('span', 'badge', d.api + ' API'));
  body.appendChild(status);

  const section = (title, pairs) => {
    body.appendChild(el('h4', null, title));
    const dl = el('dl', 'dl-grid');
    for (const [k, v] of pairs) {
      dl.appendChild(el('dt', null, k));
      dl.appendChild(el('dd', null, String(v)));
    }
    body.appendChild(dl);
  };

  section('Identity', [
    ['Fingerprint', d.fp],
    ['Identified by', d.via || 'unknown'],
    ['Client', d.clientLabel || 'unknown'],
    ['API surface', d.api || '—'],
    ['Compactions', d.compactions],
    ['Tracked hashes', d.trackedHashes],
  ]);

  section('Activity', [
    ['Turns', fmtInt(d.turns)],
    ['Failed attempts', fmtInt(d.errors)],
    ['Age', fmtDuration(d.ageSecs)],
    ['Idle', fmtDuration(d.idleSecs)],
    ['Turns / min', d.turnsPerMinute],
    ['Slowest turn', d.maxLatencyMs ? `${fmtInt(d.maxLatencyMs)} ms` : '—'],
  ]);

  section('Cost & tokens', [
    ['Total cost', fmtMoney(d.cost)],
    ['Cost / turn', fmtMoney(d.costPerTurn)],
    ['Input tokens', fmtInt(d.inTokens)],
    ['Output tokens', fmtInt(d.outTokens)],
    ['Tokens / turn', fmtInt(d.tokensPerTurn)],
    ['Data moved', fmtBytes(d.bytesTotal)],
  ]);

  section('Routing', [
    ['Provider', d.provider],
    ['Key', d.key],
    ['Key switches', fmtInt(d.keySwitches)],
    ['Models', Object.entries(d.models || {}).map(([m, n]) => `${m} x${n}`).join(', ') || '—'],
  ]);

  if ((d.errorList || []).length) {
    body.appendChild(el('h4', null, `Failures in this session (${d.errorList.length})`));
    const wrap = el('div', 'table-scroll');
    const tbl = el('table');
    const thead = el('thead');
    thead.innerHTML = '<tr><th>Time</th><th>Class</th><th>Provider</th><th class="num">HTTP</th><th>Action</th></tr>';
    tbl.appendChild(thead);
    const tbody = el('tbody');
    for (const e of d.errorList) {
      const tr = el('tr');
      tr.appendChild(el('td', 'mono', fmtClock(e.t)));
      const tc = el('td');
      tc.appendChild(el('span', 'badge ' + (ERR_TONE[e.class] || ''), e.class));
      tr.appendChild(tc);
      tr.appendChild(el('td', null, e.provider));
      tr.appendChild(el('td', 'num', e.status || 'transport'));
      tr.appendChild(el('td', null, e.action));
      tbody.appendChild(tr);
    }
    tbl.appendChild(tbody);
    wrap.appendChild(tbl);
    body.appendChild(wrap);
  }
}

function closeSession() {
  $('#sdDrawer').hidden = true;
  $('#sdBackdrop').hidden = true;
  // Restore focus to whatever opened the drawer (WCAG focus management).
  if (sdLastFocus && sdLastFocus.isConnected) sdLastFocus.focus();
}

function initDrawer() {
  $('#sdClose')?.addEventListener('click', closeSession);
  $('#sdBackdrop')?.addEventListener('click', closeSession);
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !$('#sdDrawer').hidden) closeSession();
  });
}

/* ── toasts ─────────────────────────────────────────────────────────────── */

function toast(kind, msg) {
  const wrap = $('#toasts');
  const t = el('div', 'toast');
  t.dataset.kind = kind === 'ok' ? 'ok' : kind === 'error' ? 'error' : 'warn';
  t.innerHTML = kind === 'ok' ? ICON.check : kind === 'error' ? ICON.x : ICON.warn;
  const svg = t.querySelector('svg');
  if (svg) svg.style.cssText = 'width:18px;height:18px';
  t.appendChild(el('span', null, msg));
  wrap.appendChild(t);
  // Auto-dismiss in 4s (skill: toast-dismiss 3-5s).
  setTimeout(() => {
    t.classList.add('leaving');
    setTimeout(() => t.remove(), 220);
  }, 4000);
}

/* ── sound toggle ───────────────────────────────────────────────────────── */

function initSound() {
  const btn = $('#soundBtn');
  const on = $('#soundOn');
  const off = $('#soundOff');
  const label = $('#soundLabel');

  // Persisted, but audio still requires a gesture, so we only *arm* it here.
  const want = localStorage.getItem('tabi.alerts') === '1';

  const paint = (enabled) => {
    btn.setAttribute('aria-pressed', String(enabled));
    on.hidden = !enabled;
    off.hidden = enabled;
    label.textContent = enabled ? 'Alerts on' : 'Alerts off';
  };

  btn.addEventListener('click', () => {
    if (chime.enabled) {
      chime.disable();
      localStorage.setItem('tabi.alerts', '0');
      paint(false);
    } else {
      const ok = chime.enable();
      if (!ok) {
        toast('warn', 'This browser will not allow audio alerts');
        return;
      }
      localStorage.setItem('tabi.alerts', '1');
      paint(true);
      chime.play(false); // confirm audibly that it works
    }
  });

  paint(false);
  if (want) {
    // Arm on the first interaction anywhere, since autoplay is blocked.
    const arm = () => {
      if (chime.enable()) paint(true);
      window.removeEventListener('pointerdown', arm);
      window.removeEventListener('keydown', arm);
    };
    window.addEventListener('pointerdown', arm, { once: true });
    window.addEventListener('keydown', arm, { once: true });
  }
}

/* ── boot ───────────────────────────────────────────────────────────────── */

/* ══ APPEARANCE PANEL ══════════════════════════════════════════════════════
 * A popover rather than a modal: preferences must not take over the page or
 * break the back button (skill: modal-vs-navigation). It still traps focus while
 * open, and returns focus to the trigger on close, so a keyboard user is never
 * left behind an invisible layer.
 * ═══════════════════════════════════════════════════════════════════════════ */

function initAppearance() {
  const btn = $('#appearanceBtn');
  const panel = $('#appearancePanel');
  const backdrop = $('#panelBackdrop');
  if (!btn || !panel || !backdrop) return;

  // Reflect saved values into the radios before anything can be clicked.
  const groups = {
    theme: '#setTheme', accent: '#setAccent', density: '#setDensity',
    motion: '#setMotion', quality: '#setQuality', speed: '#setSpeed',
  };
  const syncInputs = () => {
    for (const [key, sel] of Object.entries(groups)) {
      const box = $(sel);
      if (!box) continue;
      for (const input of box.querySelectorAll('input')) {
        input.checked = input.value === settings[key];
      }
    }
    // Say when the OS is overriding the motion choice, rather than looking broken.
    const note = $('#motionNote');
    if (note) {
      note.textContent = window.matchMedia('(prefers-reduced-motion: reduce)').matches
        ? 'Your system asks for reduced motion, so effects stay off regardless of this setting.'
        : 'Calm keeps transitions but drops looping effects.';
    }
    const sn = $('#speedNote');
    if (sn) {
      const ms = SPEED_MS[settings.speed] ?? SPEED_MS.moderate;
      sn.textContent = `Repaints at most every ${ms < 1000 ? ms + 'ms' : ms / 1000 + 's'}. The gateway still pushes every change; this only limits redraw work.`;
    }
  };

  for (const [key, sel] of Object.entries(groups)) {
    const box = $(sel);
    if (!box) continue;
    box.addEventListener('change', (e) => {
      const v = e.target?.value;
      if (!v || !(VALID[key] || []).includes(v)) return;
      settings[key] = v;
      saveSettings();
      applySettings();
      syncInputs();
      // Speed changes must take effect now, not after the old timer expires.
      if (key === 'speed') {
        clearTimeout(state.renderTimer);
        state.renderTimer = null;
        scheduleRender();
      }
    });
  }

  let lastFocus = null;
  const open = () => {
    lastFocus = document.activeElement;
    syncInputs();
    panel.hidden = false;
    backdrop.hidden = false;
    btn.setAttribute('aria-expanded', 'true');
    // Focus the first checked radio so arrow keys immediately move within the
    // theme group. `querySelector('input:checked, button')` would match the
    // close button first (it appears earlier in the DOM), which puts a keyboard
    // user one Tab away from dismissing the panel they just opened.
    (panel.querySelector('input:checked') || $('#panelClose'))?.focus();
  };
  const close = () => {
    panel.hidden = true;
    backdrop.hidden = true;
    btn.setAttribute('aria-expanded', 'false');
    // Return focus to the trigger (skill: escape-routes).
    //
    // `lastFocus` is only trusted when it is a real focusable element OUTSIDE
    // the panel. A mouse click does not focus a button in every engine (Safari
    // notably), so lastFocus is often <body> — restoring that would leave focus
    // at the top of the document, and restoring something inside the now-hidden
    // panel would drop focus into nothing.
    const restore =
      lastFocus &&
      lastFocus !== document.body &&
      document.contains(lastFocus) &&
      !panel.contains(lastFocus)
        ? lastFocus
        : btn;
    restore.focus();
  };

  btn.addEventListener('click', () => (panel.hidden ? open() : close()));
  $('#panelClose')?.addEventListener('click', close);
  backdrop.addEventListener('click', close);

  panel.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      e.stopPropagation();
      close();
      return;
    }
    if (e.key !== 'Tab') return;
    // Focus trap. Radios are only tabbable when checked, so the list is computed
    // live rather than cached.
    const focusables = Array.from(
      panel.querySelectorAll('button, input:checked, [tabindex]:not([tabindex="-1"])'),
    ).filter((n) => n.offsetParent !== null);
    if (!focusables.length) return;
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  });

  $('#resetDisplay')?.addEventListener('click', () => {
    Object.assign(settings, DEFAULTS);
    saveSettings();
    applySettings();
    syncInputs();
    state.range.req = DEFAULTS.range;
    state.range.byte = DEFAULTS.range;
    initRange('req', () => reqChart);
    initRange('byte', () => byteChart);
    toast('ok', 'Display settings reset');
  });

  // A system-level motion preference change should be honoured immediately.
  window.matchMedia('(prefers-reduced-motion: reduce)').addEventListener?.('change', () => {
    syncInputs();
    applySettings();
  });

  syncInputs();
}

/* ══ SPOTLIGHT ═════════════════════════════════════════════════════════════
 * Pointer-tracking radial on cards. Delegated (one listener, not one per card),
 * rAF-throttled, and only bound on a fine pointer with full motion — on touch
 * there is nothing to track and it would repaint on every scroll frame.
 * ═══════════════════════════════════════════════════════════════════════════ */

function initSpotlight() {
  if (!window.matchMedia('(hover: hover) and (pointer: fine)').matches) return;
  let raf = 0;
  let pending = null;
  document.addEventListener(
    'pointermove',
    (e) => {
      if (settings.motion !== 'full' || motionOff()) return;
      const card = e.target instanceof Element ? e.target.closest('.card.spot') : null;
      if (!card) return;
      pending = { card, x: e.clientX, y: e.clientY };
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        if (!pending) return;
        const { card: c, x, y } = pending;
        const r = c.getBoundingClientRect();
        c.style.setProperty('--mx', `${((x - r.left) / r.width) * 100}%`);
        c.style.setProperty('--my', `${((y - r.top) / r.height) * 100}%`);
      });
    },
    { passive: true },
  );
}

function boot() {
  applySettings();
  initCharts();
  initTabs();
  initModelSort();
  initKeyForm();
  initSound();
  initProxyControls();
  initSettingsPage();
  initDrawer();
  initAppearance();
  initSpotlight();
  initRange('req', () => reqChart);
  initRange('byte', () => byteChart);
  connect();
  startTicker();

  // Refresh only relative timestamps once a minute. Cheap, and avoids the whole
  // page re-rendering just to age a label.
  setInterval(() => {
    if (document.hidden || !state.snap) return;
    if (state.page === 'sessions') renderSessions(state.snap);
    if (state.page === 'proxies') renderProxies();
  }, 60000);
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', boot);
} else {
  boot();
}
