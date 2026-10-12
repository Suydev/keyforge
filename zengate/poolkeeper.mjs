// poolkeeper — background harvester for keyforge's zen egress pool.
//
// zen's free tier limits PER-EGRESS-IP. So the job is not "find one good
// proxy" but "keep a steady supply of verified, fast addresses", because free
// proxies rot within minutes (measured: 32 loaded, 17 dead inside 20 minutes).
//
// This daemon, on a timer:
//   1. pulls several public proxy lists,
//   2. screens them for a CONNECT tunnel to opencode.ai:443 (bounded workers),
//   3. proves survivors with a REAL zen chat pong (full fingerprint + the four
//      gate tools) — nothing is accepted on a /v1/models 200,
//   4. keeps only pongs faster than MAX_TTFB_MS, merged with the pool we
//      already have and ranked fastest-first,
//   5. writes proxies.txt in keyforge's format and hot-reloads the gateway.
//
// Env: POOL_FILE, KEYFORGE_URL, INTERVAL_MIN, MAX_TTFB_MS, CONNECT_W, VET_W
import fs from 'node:fs';
import net from 'node:net';
import tls from 'node:tls';
import https from 'node:https';
import http from 'node:http';
import crypto from 'node:crypto';

const POOL_FILE   = process.env.POOL_FILE   || '/data/data/com.termux/files/home/git_gorouter_keyforge-token/proxies.txt';
const KEYFORGE_URL= process.env.KEYFORGE_URL|| 'http://127.0.0.1:8787';
const LOG         = process.env.POOL_LOG    || '/data/data/com.termux/files/home/tmp/poolkeeper.log';
const INTERVAL_MIN= +(process.env.INTERVAL_MIN || 30);
const MAX_TTFB_MS = +(process.env.MAX_TTFB_MS || 4000);
const CONNECT_W   = +(process.env.CONNECT_W   || 20);
const VET_W       = +(process.env.VET_W       || 40);
const MAX_TUNNEL  = +(process.env.MAX_TUNNEL  || 700);
const MODEL       = process.env.MODEL        || 'big-pickle';

const ZEN_HOST = 'opencode.ai';
const SOURCES = [
  { kind:'json', url:'https://raw.githubusercontent.com/proxifly/free-proxy-list/main/proxies/protocols/http/data.json' },
  { kind:'json', url:'https://raw.githubusercontent.com/proxifly/free-proxy-list/main/proxies/protocols/https/data.json' },
  { kind:'txt',  url:'https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/http.txt' },
  { kind:'txt',  url:'https://raw.githubusercontent.com/monosans/proxy-list/main/proxies/http.txt' },
  { kind:'txt',  url:'https://raw.githubusercontent.com/roosterkid/openproxylist/main/HTTPS_RAW.txt' },
  { kind:'txt',  url:'https://raw.githubusercontent.com/jetkai/proxy-list/main/online-proxies/txt/proxies-http.txt' },
  { kind:'txt',  url:'https://raw.githubusercontent.com/jetkai/proxy-list/main/online-proxies/txt/proxies-https.txt' },
];
const GATE = ['bash','glob','grep','read'];

const hex = n => crypto.randomBytes(Math.ceil(n/2)).toString('hex').slice(0,n);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const log = (...a) => {
  const line = new Date().toISOString().slice(11,19) + ' ' + a.join(' ');
  try { fs.appendFileSync(LOG, line + '\n'); } catch {}
};

// ── concurrency helper: run fn over items with N in flight ──────────────────
async function pmap(items, n, fn) {
  let i = 0; const out = [];
  await Promise.all(Array.from({ length: n }, async () => {
    while (i < items.length) { const k = i++; out[k] = await fn(items[k], k); }
  }));
  return out;
}

// ── source loading ───────────────────────────────────────────────────────────
function fetchBuf(url) {
  return new Promise((res, rej) => {
    https.get(url, { headers:{ 'User-Agent':'poolkeeper/1.0' }, timeout: 45000 }, r => {
      if (r.statusCode !== 200) { r.resume(); return rej(new Error('HTTP ' + r.statusCode)); }
      const c = []; r.on('data', d => c.push(d));
      r.on('end', () => res(Buffer.concat(c)));
    }).on('error', rej).on('timeout', function(){ this.destroy(new Error('timeout')); });
  });
}

async function loadCandidates() {
  const seen = new Set(); const out = [];
  for (const s of SOURCES) {
    let raw;
    try { raw = await fetchBuf(s.url); }
    catch (e) { log('source fail', s.url.split('/').slice(-2).join('/'), e.message); continue; }
    let n = 0;
    if (s.kind === 'json') {
      let arr; try { arr = JSON.parse(raw.toString()); } catch { continue; }
      for (const e of arr) {
        const ip = e.ip, port = e.port;
        if (!ip || seen.has(ip) || !Number.isInteger(port) || port <= 0 || port > 65535) continue;
        seen.add(ip); out.push([ip, port]); n++;
      }
    } else {
      for (const line of raw.toString().split('\n')) {
        const t = line.trim(); if (!t || !t.includes(':')) continue;
        const k = t.lastIndexOf(':'); const ip = t.slice(0,k), port = Number(t.slice(k+1));
        if (!ip || seen.has(ip) || !Number.isInteger(port) || port <= 0 || port > 65535) continue;
        seen.add(ip); out.push([ip, port]); n++;
      }
    }
    log('  +' + n, s.url.split('/').slice(-1)[0]);
  }
  return out;
}

// ── stage 1: can this address reach zen at all? ──────────────────────────────
function connectOk([ip, port]) {
  return new Promise(res => {
    const s = net.connect(port, ip, () => {
      s.write('CONNECT ' + ZEN_HOST + ':443 HTTP/1.1\r\nHost: ' + ZEN_HOST + ':443\r\n\r\n');
    });
    let buf = '';
    const on = d => {
      buf += d.toString('latin1');
      if (buf.includes('\r\n\r\n')) { s.off('data', on); s.destroy(); res(/^HTTP\/1\.1 200/.test(buf)); }
    };
    s.on('data', on);
    s.on('error', () => res(false));
    setTimeout(() => { s.destroy(); res(false); }, 6000);
  });
}

// ── stage 2: prove it with a real zen pong, and time it ──────────────────────
function pong([ip, port]) {
  return new Promise(res => {
    const ses = 'ses_' + hex(12) + hex(14), req = 'msg_' + hex(12) + hex(14).toUpperCase();
    const tools = GATE.map(n => ({ type:'function', function:{ name:n, description:'x',
      parameters:{ type:'object', properties:{}, required:[] } } }));
    const body = Buffer.from(JSON.stringify({ model: MODEL, stream:true, max_tokens:16,
      messages:[{ role:'user', content:'Reply pong' }], tools }));
    const head = [
      'POST /zen/v1/chat/completions HTTP/1.1', 'Host: ' + ZEN_HOST,
      'User-Agent: opencode/1.19.0 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14',
      'Authorization: Bearer public', 'Content-Type: application/json',
      'x-opencode-session: ' + ses, 'x-opencode-request: ' + req,
      'x-opencode-client: cli', 'x-opencode-project: global', 'Accept-Encoding: identity',
      'Content-Length: ' + body.length, '', '' ].join('\r\n');

    const t0 = Date.now();
    const s = net.connect(port, ip, () => {
      s.write('CONNECT ' + ZEN_HOST + ':443 HTTP/1.1\r\nHost: ' + ZEN_HOST + ':443\r\n\r\n');
    });
    let buf = '', upgraded = false;
    const fail = () => { try { s.destroy(); } catch {} res(null); };
    s.on('error', fail);
    s.setTimeout(20000, fail);
    s.on('data', function onData(d) {
      if (upgraded) return;
      buf += d.toString('latin1');
      if (!/^HTTP\/1\.1 200/.test(buf)) { if (buf.includes('\r\n\r\n')) fail(); return; }
      if (!buf.includes('\r\n\r\n')) return;
      s.off('data', onData); upgraded = true;
      const secure = tls.connect({ socket: s, servername: ZEN_HOST }, () => {
        secure.setTimeout(20000, fail);
        secure.write(head + body);
        let r = '';
        secure.on('data', c => {
          r += c.toString('utf8');
          if (r.includes('\r\n\r\n') && r.slice(r.indexOf('\r\n\r\n')+4).includes('data:'))
            { try { secure.destroy(); } catch {} res(Date.now() - t0); }
        });
        secure.on('error', fail);
        secure.on('end', () => { if (r.includes('data:')) res(Date.now() - t0); else fail(); });
      });
      secure.on('error', fail);
    });
  });
}

// ── pool file I/O (keyforge format, TTFB-ranked, vetted) ──────────────────────
function readPool() {
  const m = new Map();
  try {
    for (const line of fs.readFileSync(POOL_FILE, 'utf8').split('\n')) {
      const t = line.trim(); if (!t || t.startsWith('#')) continue;
      const addr = t.split(/\s+/)[0];
      let ms = 99999;
      if (t.includes('#')) for (const tok of t.split('#')[1].split(/\s+/))
        if (tok.endsWith('ms') && /^\d+ms$/.test(tok)) ms = +tok.slice(0,-2);
      m.set(addr, Math.min(m.get(addr) ?? 99999, ms));
    }
  } catch {}
  return m;
}

function writePool(m) {
  const rows = [...m.entries()].map(([a, ms]) => [ms, a]).sort((x, y) => x[0] - y[0]);
  const now = Math.floor(Date.now() / 1000);
  const body = rows.map(([ms, a]) => `${a}  # ok=1 fail=0 ${ms}ms vetted=${now}`).join('\n');
  const out = '# keyforge proxy pool — TTFB-ranked (fastest first), multi-source, ping-verified.\n' +
              '# Each entry streamed a real ' + MODEL + ' pong through that exact IP.\n' + body + '\n';
  const tmp = POOL_FILE + '.tmp';
  fs.writeFileSync(tmp, out);
  fs.renameSync(tmp, POOL_FILE);   // atomic: keyforge never reads a half-written file
  return rows.length;
}

// ── one harvest cycle ────────────────────────────────────────────────────────
async function cycle() {
  log('harvest: start');
  const cands = await loadCandidates();
  log('harvest: ' + cands.length + ' candidates');
  if (!cands.length) return 0;

  const tunnel = [];
  await pmap(cands, CONNECT_W, async c => {
    if (await connectOk(c)) { tunnel.push(c); }
  });
  log('harvest: ' + tunnel.length + ' tunnel ok');

  const timed = await pmap(tunnel, VET_W, pong);
  const fresh = [];
  timed.forEach((ms, i) => { if (ms != null && ms <= MAX_TTFB_MS) fresh.push([ms, tunnel[i]]); });
  fresh.sort((a, b) => a[0] - b[0]);
  log('harvest: ' + fresh.length + ' fast pongs (<=' + MAX_TTFB_MS + 'ms), best ' + (fresh[0]?.[0] ?? '-') + 'ms');
  for (const [ms, [ip, port]] of fresh.slice(0, 5)) log('   ' + ms + 'ms ' + ip + ':' + port);

  const merged = readPool();
  for (const [ms, [ip, port]] of fresh) {
    const a = ip + ':' + port;
    merged.set(a, Math.min(ms, merged.get(a) ?? 99999));
  }
  const n = writePool(merged);
  log('harvest: wrote ' + n + ' ranked proxies');

  // hot-reload the gateway so the new pool takes effect without a restart
  await reloadKeyforge();
  return fresh.length;
}

// Tell the running gateway to re-read the pool file, so a harvest takes effect
// without a restart. Best-effort: a gateway that is down must not fail the cycle.
function reloadKeyforge() {
  return new Promise(res => {
    const u = new URL('/api/proxies/reload', KEYFORGE_URL);
    const mod = u.protocol === 'https:' ? https : http;
    const r = mod.request(
      { hostname: u.hostname, port: u.port || (u.protocol === 'https:' ? 443 : 80),
        path: u.pathname, method: 'POST', timeout: 8000 },
      () => res()
    );
    r.on('error', () => res());
    r.on('timeout', () => { r.destroy(); res(); });
    r.end();
  });
}

// ── daemon loop ──────────────────────────────────────────────────────────────
log('poolkeeper up — interval ' + INTERVAL_MIN + 'm, cap ' + MAX_TTFB_MS + 'ms, pool ' + POOL_FILE);
await cycle();
while (true) {
  await sleep(INTERVAL_MIN * 60000);
  try { await cycle(); } catch (e) { log('cycle error: ' + (e && e.message)); }
}
