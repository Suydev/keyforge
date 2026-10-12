#!/usr/bin/env node
// zengate — OpenCode Zen free-tier gateway with a self-healing IP pool.
//
// Why this exists: zen's free limit is PER-EGRESS-IP, not per-session. A new
// session id does not clear it (proved: fresh ses_ through a burnt IP still
// 429'd). What clears it is a different source address. So this gateway keeps
// a large pool of verified egress IPs, pins one conversation to one IP, and on
// zen's 429 cools that IP for the upstream's real retry-after, rolls the
// session id, and rotates to a fresh IP — while keeping the request alive.
//
// Run:  node gate.mjs            (default :8790)
// Env:  PORT, POOL, MAX_TTFB_MS, PROXY_ROTATE_MS, OFFLINE_HOLD_MS

import http from 'node:http';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
import https from 'node:https';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const PORT   = +(process.env.PORT || 8790);
const POOL   = +(process.env.POOL || 200);          // target verified IPs
const MAX_TTFB_MS = +(process.env.MAX_TTFB_MS || 9000);
const ROTATE_MS    = +(process.env.PROXY_ROTATE_MS || 900000); // harvest cadence
const OFFLINE_HOLD_MS = +(process.env.OFFLINE_HOLD_MS || 5000);

const ZEN_HOST = 'opencode.ai';
const ZEN_BASE = `https://${ZEN_HOST}/zen/v1`;
const SOURCES = [
  'https://raw.githubusercontent.com/proxifly/free-proxy-list/main/proxies/protocols/http/data.json',
  'https://raw.githubusercontent.com/proxifly/free-proxy-list/main/proxies/protocols/https/data.json',
];
const GATE_TOOLS = ['bash','glob','grep','read'];
const MODEL_POOL = ['big-pickle','space-bunny-free','mimo-v2.6-flash-free'];

const hex = n => crypto.randomBytes(Math.ceil(n/2)).toString('hex').slice(0,n);
const newSessionId = () => `ses_${hex(12)}${hex(14)}`;
const newRequestId = () => `msg_${hex(12)}${hex(14).toUpperCase()}`;
const LOGFILE = process.env.ZENGATE_LOG || "/data/data/com.termux/files/home/tmp/zengate.log";
const log = (...a) => { const l = new Date().toISOString().slice(11,19)+" "+a.join(" "); try { fs.appendFileSync(LOGFILE, l+"\n"); } catch {} };

// ─────────────────────────── the IP pool ───────────────────────────
// An IP is only ever routed on after it streamed a real zen pong, and it is
// ranked by TTFB so the fastest address is tried first.
const pool = new Map();   // addr -> {ttfb, ok, fail, coolUntil, lastUsed, vouched}

function poolHealthy() {
  const now = Date.now();
  return [...pool.values()].filter(p => p.coolUntil <= now).length;
}

/** Least-recently-used among un-cooled, preferring the fastest TTFB. */
function pickEgress(pinnedAddr) {
  const now = Date.now();
  if (pinnedAddr) {
    const p = pool.get(pinnedAddr);
    if (p && p.coolUntil <= now) return { addr: pinnedAddr, p };
  }
  let best = null;
  for (const [addr, p] of pool) {
    if (p.coolUntil > now) continue;
    const score = p.ttfb + (now - p.lastUsed) / 50; // fast first, then rotate
    if (!best || score < best.score) best = { addr, p, score };
  }
  if (best) best.p.lastUsed = now;
  return best ? { addr: best.addr, p: best.p } : null;
}

// ────────────────────────── zen fingerprint ──────────────────────────
function zenHeaders(ses, req) {
  return {
    'User-Agent': 'opencode/1.19.0 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14',
    'Authorization': 'Bearer public',
    'Content-Type': 'application/json',
    'x-opencode-session': ses,
    'x-opencode-request': req,
    'x-opencode-client': 'cli',
    'x-opencode-project': 'global',
    'Accept-Encoding': 'identity',
  };
}
const gateTools = () => GATE_TOOLS.map(n => ({
  type:'function',
  function:{ name:n, description:n, parameters:{ type:'object', properties:{}, required:[] } },
}));

// ───────────────────── proxy tunnel (CONNECT) + TTFB ─────────────────────
function viaProxy(addr, urlStr, headers, body, { measure=false, timeoutMs=25000 }={}) {
  return new Promise(resolve => {
    const t0 = Date.now();
    const [ip, portStr] = addr.split(':');
    const port = +portStr;
    const u = new URL(urlStr);
    const req = https.request({
      host: u.hostname, port: u.port || 443, path: u.pathname + u.search,
      method: 'POST', headers: { ...headers, 'Content-Length': Buffer.byteLength(body) },
      agent: new https.Agent({ keepAlive: false }),
      timeout: timeoutMs,
    }, res => {
      if (measure) {
        const ttfb = Date.now() - t0;
        const chunks = [];
        res.on('data', d => { if (chunks.length < 6) chunks.push(d); });
        res.on('end', () => resolve({ ok:true, ttfb, status:res.statusCode,
          text: Buffer.concat(chunks).toString('utf8').slice(0,400) }));
        return;
      }
      resolve({ ok:true, passthrough:res });
    });
    req.on('timeout', () => { req.destroy(new Error('timeout')); });
    req.on('error', e => resolve({ ok:false, error:e.message }));
    req.write(body); req.end();
  });
}

/** Raw CONNECT tunnel so we can stream through a proxy without extra deps. */
function tunnelRequest(addr, urlStr, headers, body) {
  return new Promise((resolve, reject) => {
    const net = require('node:net');
    const [ip, port] = addr.split(':');
    const u = new URL(urlStr);
    const sock = net.connect(+port, ip, () => {
      sock.write(`CONNECT ${u.hostname}:443 HTTP/1.1\r\nHost: ${u.hostname}\r\nProxy-Connection: Keep-Alive\r\n\r\n`);
    });
    let buf = '';
    const onData = d => {
      buf += d.toString('latin1');
      if (buf.includes('\r\n\r\n')) {
        sock.off('data', onData);
        if (!/^HTTP\/1\.1 200/.test(buf)) { sock.destroy(); return reject(new Error('CONNECT refused')); }
        // TLS over the tunnel
        const tls = require('node:tls');
        const secure = tls.connect({ socket: sock, servername: u.hostname }, () => {
          const req = https.request({
            host: u.hostname, port: 443, path: u.pathname + u.search, method:'POST',
            headers: { ...headers, 'Content-Length': Buffer.byteLength(body) },
            createConnection: () => secure, agent: false,
          }, res => { secure.removeAllListeners('data'); resolve({ ok:true, passthrough:res }); });
          req.on('error', reject);
          req.write(body); req.end();
        });
        secure.on('error', reject);
      }
    };
    sock.on('data', onData);
    sock.on('error', reject);
    setTimeout(() => { sock.destroy(); reject(new Error('tunnel timeout')); }, 15000);
  });
}


// ───────────────────────── harvesting / vetting ─────────────────────────
async function fetchJSON(url) {
  return new Promise((resolve, reject) => {
    https.get(url, { headers:{'User-Agent':'zengate/1.0'}, timeout: 45000 }, res => {
      if (res.statusCode !== 200) { res.resume(); return reject(new Error('HTTP '+res.statusCode)); }
      const c=[]; res.on('data',d=>c.push(d));
      res.on('end',()=>{ try { resolve(JSON.parse(Buffer.concat(c).toString())); } catch(e){ reject(e);} });
    }).on('error', reject);
  });
}

async function connectOk(ip, port) {
  return new Promise(resolve => {
    const net = require('node:net');
    const sock = net.connect(+port, ip, () => {
      sock.write(`CONNECT ${ZEN_HOST}:443 HTTP/1.1\r\nHost: ${ZEN_HOST}:443\r\n\r\n`);
    });
    let buf='';
    const onD = d => { buf += d.toString('latin1'); if (buf.includes('\r\n\r\n')) { sock.off('data',onD); sock.destroy(); resolve(/^HTTP\/1\.1 200/.test(buf)); } };
    sock.on('data', onD);
    sock.on('error', () => resolve(false));
    setTimeout(() => { sock.destroy(); resolve(false); }, 6000);
  });
}

/** Real zen pong through one IP. Returns ttfb ms if it truly answered. */
async function pongProbe(ip, port, model) {
  const body = JSON.stringify({ model, stream:true, max_tokens:16,
    messages:[{role:'user',content:'Reply pong'}], tools: gateTools() });
  const r = await viaProxy(`${ip}:${port}`, `${ZEN_BASE}/chat/completions`, zenHeaders(newSessionId(), newRequestId()), body, { measure:true, timeoutMs: 20000 }).catch(()=>({ok:false}));
  if (!r.ok) return null;
  if (r.status === 200 && r.text.includes('data:')) return r.ttfb;
  if (r.status === 429) return '429';
  return null;
}

async function harvestOnce(model) {
  const seen = new Set(); const cands = [];
  for (const src of SOURCES) {
    let raw; try { raw = await fetchJSON(src); } catch { continue; }
    for (const e of raw) {
      const ip=e.ip, port=e.port;
      if (!ip || seen.has(ip) || !Number.isInteger(port) || port<=0 || port>65535) continue;
      seen.add(ip); cands.push([ip,port]);
    }
  }
  log(`harvest: ${cands.length} candidates`);
  // pre-screen CONNECT concurrently
  // Bounded concurrency. Firing all ~5.5k CONNECT probes at once exhausts the
  // process's sockets, so every probe fails and the harvest reports "0 tunnel
  // ok" against a list that is known to contain hundreds of working proxies.
  const tunneled=[];
  const CONC=60;
  let idx=0;
  await Promise.all(Array.from({length:CONC}, async () => {
    while (idx < cands.length && tunneled.length < 600) {
      const [ip,port] = cands[idx++];
      if (await connectOk(ip,port)) tunneled.push([ip,port]);
    }
  }));
  log(`harvest: ${tunneled.length} tunnel ok`);
  // pong-verify, keep only fast ones
  const workers = 10; let vidx = 0; const found=[];
  await Promise.all(Array.from({length:workers}, async () => {
    while (vidx < tunneled.length) {
      const [ip,port] = tunneled[vidx++];
      const ttfb = await pongProbe(ip,port,model);
      if (typeof ttfb === 'number' && ttfb <= MAX_TTFB_MS) {
        pool.set(`${ip}:${port}`, { ttfb, ok:1, fail:0, coolUntil:0, lastUsed:0, vouched:Date.now() });
        found.push([ttfb, `${ip}:${port}`]);
      }
    }
  }));
  found.sort((a,b)=>a[0]-b[0]);
  log(`harvest: ${found.length} fast pongs (best ${found[0]?.[0]}ms)`);
  for (const [ms,a] of found.slice(0,10)) log(`   ${ms}ms ${a}`);
  return found.length;
}

async function harvestLoop() {
  let n = 0;
  while (poolHealthy() < POOL) {
    const got = await harvestOnce(MODEL_POOL[n++ % MODEL_POOL.length]);
    if (poolHealthy() >= POOL) break;
    if (!got) { log('harvest: nothing passed, retrying in 45s'); await sleep(45000); }
  }
  log(`harvest: pool at ${poolHealthy()} healthy`);
  while (true) { await sleep(ROTATE_MS); await harvestOnce('big-pickle'); }
}
const sleep = ms => new Promise(r=>setTimeout(r,ms));

// ─────────────────────────── the gateway ───────────────────────────
// One conversation = one IP, so a session owns that address's free budget
// instead of the whole pool burning it.
const sessions = new Map(); // fp -> { ses, egress }

function fpOf(ip, body) {
  const f = ip + '|' + (body?.messages?.[0]?.content ?? '');
  return crypto.createHash('fnv1a').update(f.slice(0,600)).digest('hex').slice(0,16);
}

function toZenBody(body) {
  // minimal: ensure stream:true + gate tools + text content (what zen needs)
  const msgs = (body.messages||[]).map(m => ({
    role: m.role,
    content: typeof m.content === 'string' ? m.content
      : (Array.isArray(m.content) ? m.content.filter(c=>c.type==='text').map(c=>c.text).join('') : ''),
  }));
  const tools = [...(body.tools||[]), ...gateTools()];
  return JSON.stringify({ model: body.model, stream:true, stream_options:{include_usage:true},
    max_tokens: body.max_tokens || 64000, messages: msgs, tools });
}

async function serveToZen(fp, bodyObj, res, anthropicIn) {
  const sess = sessions.get(fp) || (sessions.set(fp, (sessions.get(fp)||{ses:newSessionId(), egress:null})), sessions.get(fp));
  for (let attempt = 0; attempt < 12; attempt++) {
    const egress = pickEgress(sess.egress);
    const zenBody = toZenBody(bodyObj);
    if (!egress) { // every pooled IP is cooling → try direct
      await viaDirect(zenBody, sess, res, anthropicIn);
      return;
    }
    let r;
    try { r = await tunnelRequest(egress.addr, `${ZEN_BASE}/chat/completions`, zenHeaders(sess.ses, newRequestId()), zenBody); }
    catch (e) { egress.p.fail++; egress.p.coolUntil = Date.now()+60000; if (egress.p.fail>=3) pool.delete(egress.addr); sess.egress=null; continue; }

    // read status head
    const status = r.passthrough.statusCode;
    if (status === 200) { sess.egress = egress.addr; egress.p.ok++; egress.p.ttfb = Math.min(egress.p.ttfb||9999, 9999); streamThrough(r.passthrough, res, anthropicIn); return; }
    const text = await drain(r.passthrough);
    if (status === 429 && text.includes('FreeUsageLimitError')) {
      // per-IP budget spent: cool THIS ip for the real window, roll session, rotate
      const ra = +(/retry-after:\s*(\d+)/i.exec(text.match(/retry-after[^\n]*/)?.[0]||'')?.[1] || 900);
      egress.p.coolUntil = Date.now() + Math.min(ra*1000, 2*3600*1000);
      sess.ses = newSessionId(); sess.egress = null;
      log(`429 on ${egress.addr} — cooled ${Math.min(ra,7200)}s, rotating ip`);
      continue;
    }
    // any other failure → this proxy is bad
    egress.p.fail++; egress.p.coolUntil = Date.now()+30000; sess.egress=null;
    if (egress.p.fail>=3) pool.delete(egress.addr);
  }
  res.writeHead(502, {'content-type':'application/json'});
  res.end(JSON.stringify({type:'error', error:{type:'upstream', message:'no working zen egress right now'}}));
}

// zen's wire format is OpenAI SSE. A caller on /v1/messages expects Anthropic
// frames, so translate rather than leaking a foreign stream shape to it.
function streamThrough(up, res, anthropicIn) {
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  if (!anthropicIn) { up.pipe(res); return; }
  const ev = (name, data) => res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
  let started = false, idx = 0, buf = '', toolCalls = new Map(), stop = 'end_turn';
  const flushLines = (flush) => {
    let i;
    while ((i = buf.indexOf('\n')) >= 0 || flush) {
      if (i < 0) { if (!flush) break; i = buf.length; }
      const line = buf.slice(0, i); buf = buf.slice(i + 1);
      const m = /^data:\s*(.*)$/.exec(line.trim());
      if (!m || m[1] === '[DONE]') continue;
      let v; try { v = JSON.parse(m[1]); } catch { continue; }
      if (v.error) { ev('error', { type:'error', error:v.error }); continue; }
      const ch = v.choices?.[0]; if (!ch) continue;
      const d = ch.delta || {};
      if (!started) {
        started = true;
        ev('message_start', { type:'message_start', message:{ id: v.id||'msg_zen', type:'message', role:'assistant',
          model: v.model||'big-pickle', content:[], stop_reason:null, stop_sequence:null,
          usage:{ input_tokens:0, output_tokens:0 } } });
        ev('content_block_start', { type:'content_block_start', index:0,
          content_block:{ type:'text', text:'' } });
      }
      if (typeof d.content === 'string' && d.content) {
        ev('content_block_delta', { type:'content_block_delta', index:0,
          delta:{ type:'text_delta', text:d.content } });
      }
      for (const tc of d.tool_calls || []) {
        const k = tc.index ?? 0;
        if (tc.id) {
          toolCalls.set(k, { id: tc.id, name: tc.function?.name || '', json: '' });
          ev('content_block_start', { type:'content_block_start', index: idx + 1 + k,
            content_block:{ type:'tool_use', id: tc.id, name: tc.function?.name||'', input:{} } });
        }
        const cur = toolCalls.get(k);
        if (cur && tc.function?.arguments) {
          cur.json += tc.function.arguments;
          ev('content_block_delta', { type:'content_block_delta', index: idx + 1 + k,
            delta:{ type:'partial_json', partial_json: tc.function.arguments } });
        }
      }
      if (ch.finish_reason === 'tool_calls' || ch.finish_reason === 'function_call') stop = 'tool_use';
      else if (ch.finish_reason === 'length') stop = 'max_tokens';
    }
  };
  up.on('data', d => { buf += d.toString('utf8'); flushLines(false); });
  up.on('end', () => {
    flushLines(true);
    if (started) {
      ev('content_block_stop', { type:'content_block_stop', index:0 });
      ev('message_delta', { type:'message_delta', delta:{ stop_reason:stop, stop_sequence:null },
        usage:{ output_tokens:0 } });
      ev('message_stop', { type:'message_stop' });
    }
    res.end();
  });
  up.on('error', () => res.end());
}
async function drain(up){ const c=[]; for await (const d of up) { c.push(d); if (c.length>40) break; } return Buffer.concat(c).toString('utf8').slice(0,2000); }
// Direct egress, used when every pooled IP is cooling. This is the safety
// valve that keeps the gateway answering instead of 502-ing: a device IP that
// still has budget will serve, and one that does not fails fast with zen's own
// error rather than hanging.
async function viaDirect(zenBody, sess, res, anthropicIn) {
  const https2 = await import('node:https');
  const req = https2.request({
    host: ZEN_HOST, port: 443, path: '/zen/v1/chat/completions', method: 'POST',
    headers: zenHeaders(sess.ses, newRequestId()),
    timeout: 60000,
  }, up => { streamThrough(up, res, anthropicIn); });
  req.on('timeout', () => req.destroy(new Error('direct timeout')));
  req.on('error', e => {
    if (!res.headersSent) { res.writeHead(502, {'content-type':'application/json'}); }
    res.end(JSON.stringify({type:'error', error:{type:'api_error', message:'direct egress failed: '+e.message}}));
  });
  req.end(zenBody);
}

const server = http.createServer(async (req,res) => {
  if (req.method==='GET' && req.url==='/__pool') {
    const now=Date.now();
    const rows=[...pool.entries()].map(([a,p])=>({addr:a,ttfb:p.ttfb,ok:p.ok,fail:p.fail,cooling:p.coolUntil>now,age:now-p.vouched}))
      .sort((a,b)=>a.ttfb-b.ttfb);
    return json(res,{healthy:poolHealthy(),target:POOL,sessions:sessions.size,rows});
  }
  if (req.method==='GET' && req.url==='/__harvest') { harvestOnce('big-pickle').then(n=>log('manual harvest done:',n)); return json(res,{started:true}); }
  if (req.method==='GET' && req.url==='/__health') {
    return json(res,{ ok:true, pool:poolHealthy(), target:POOL,
      sessions:sessions.size, uptime:Math.round(process.uptime()) });
  }
  // Drop addresses that have failed repeatedly (MITM / black hole).
  if (req.method==='POST' && req.url==='/__prune') {
    let n=0; const now=Date.now();
    for (const [a,p] of pool) if (p.fail>=3 || (p.coolUntil>now && p.fail>=2)) { pool.delete(a); n++; }
    log('pruned', n, 'dead addresses; pool now', poolHealthy());
    return json(res,{ pruned:n, healthy:poolHealthy() });
  }
  if (!(req.url.includes('/chat/completions')||req.url.includes('/v1/messages'))) { res.writeHead(404); return res.end(); }

  const chunks=[]; for await (const d of req) chunks.push(d);
  let body; try { body = JSON.parse(Buffer.concat(chunks).toString()); } catch { res.writeHead(400); return res.end('bad json'); }
  const fp = fpOf(req.socket.remoteAddress, body);
  const anthropicIn = req.url.includes('/v1/messages');
  await serveToZen(fp, body, res, anthropicIn);
});
function json(res,o){ res.writeHead(200,{'content-type':'application/json'}); res.end(JSON.stringify(o)); }

server.listen(PORT, () => {
  log(`zengate on :${PORT} — harvesting a ${POOL}-IP pool of verified zen egress...`);
  harvestLoop();
});
