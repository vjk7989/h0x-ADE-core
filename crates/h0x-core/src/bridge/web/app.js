// HyperOx web panel: pair from #pair=&fp= (or a typed code), keep the session,
// list the agents and control them over the /ws API (docs/bridge.md, Web
// panel). Every piece of text from the server goes in with textContent.
// The socket is end to end encrypted (ADR-064, bridge/web/e2e.rs): the first
// frame each way is a plain hello, every later frame is a sealed binary one.
'use strict';

const $ = (id) => document.getElementById(id);
const KEY = 'h0x-session';
const PING_MS = 25000; // the server drops a connection silent for 60 s
const DEAD_MS = 50000; // it sends a heartbeat every 20 s
const MAX_LINES = 200;
const FIRST_LINES = 80;

const keep = (key) => ({
  get() { try { return localStorage.getItem(key); } catch { return null; } },
  set(v) { try { localStorage.setItem(key, v); } catch { /* private mode: this tab only */ } },
  drop() { try { localStorage.removeItem(key); } catch { /* nothing stored */ } },
});
const store = keep(KEY);
const pin = keep('h0x-fp'); // the computer's fingerprint, kept once paired

let session = store.get();
let pinned = pin.get();
// One shape for a pairing code, from the link or typed: trimmed, upper-case,
// cut at anything that can't belong to it. '' means no code.
function cleanCode(raw) {
  return String(raw || '').trim().split(/[#&\s]/)[0].toUpperCase();
}
const hash = new URLSearchParams(location.hash.slice(1));
let pairCode = cleanCode(hash.get('pair'));
const linkFp = String(hash.get('fp') || '').trim();
// The one-time code leaves the address bar (and the history) at once.
if (location.hash) history.replaceState(null, '', location.pathname + location.search);

let ws = null;
let chan = null; // the sealed channel of the current socket, after the handshake
let blocked = false; // the computer is not the one this device pinned
let usedFp = ''; // the fingerprint the last handshake matched
let tail = Promise.resolve(); // everything that touches the socket runs in this order
let ready = false; // paired or authenticated on this socket
let locked = false;
let retries = 0;
let retryTimer = 0;
let pingTimer = 0;
let lastHeard = 0;
let nextId = 1;
const pending = new Map();
let agents = [];
let current = null; // the open agent's address
let lines = FIRST_LINES;

// ---- views ----

function show(view) {
  for (const id of ['pair', 'list', 'agent']) $(id).hidden = id !== view;
  $('lock').hidden = view === 'pair';
}

function pill(text, cls) {
  $('pill').textContent = text;
  $('pill').className = `pill ${cls}`;
}

function setLocked(on) {
  locked = on;
  $('locked').hidden = !on;
  $('lock').disabled = on;
  if (on) pill('Locked', 'locked');
  else if (ready) pill('Connected', 'ok');
}

function toast(text, cls = '', terminal = null, ms = 8000) {
  const box = $('toasts');
  const el = document.createElement('div');
  el.className = `toast ${cls}`;
  const span = document.createElement('span');
  span.textContent = text;
  el.append(span);
  if (terminal) {
    const open = document.createElement('button');
    open.type = 'button';
    open.className = 'btn';
    open.textContent = 'Open';
    open.addEventListener('click', () => { el.remove(); openAgent(terminal); });
    el.append(open);
  }
  const close = document.createElement('button');
  close.type = 'button';
  close.className = 'btn ghost';
  close.textContent = '×';
  close.setAttribute('aria-label', 'Dismiss');
  close.addEventListener('click', () => el.remove());
  el.append(close);
  box.append(el);
  while (box.children.length > 4) box.firstElementChild.remove();
  if (ms) setTimeout(() => el.remove(), ms);
}

// Reached through a Cloudflare tunnel (REM-3b): say who delivers the page.
function tunnel(data) {
  if (data.tunnel) $('tunnel').hidden = false;
}

function needPair(why) {
  session = null;
  store.drop();
  ready = false;
  $('pair-why').textContent = why || '';
  $('forget').hidden = !pinned;
  pill('Not paired', '');
  show('pair');
}

// The computer is not the one this device trusts: stop, and stay stopped until
// the person forgets it (a reload with a good link also clears this).
function block(why) {
  blocked = true;
  ready = false;
  $('pair-why').textContent = why;
  $('forget').hidden = false;
  pill('Blocked', '');
  show('pair');
}

// ---- end-to-end encryption (ADR-064) ----
// The same bytes as e2e.rs and e2e_vectors.json. WebCrypto where the page has
// it; on plain-http LAN pages it does not, so noble.js (pure JS, served beside
// this file) takes over. Never a fallback to an unencrypted socket.

const enc = new TextEncoder();
const dec = new TextDecoder('utf-8', { fatal: true });
const PROTO = enc.encode('h0x-bridge v1');
const C2S = 1;
const S2C = 2;
const MAX_CTR = (1n << 64n) - 1n;
const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';

function b64(bytes) {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const chunk = bytes.subarray(i, i + 3);
    const n = (chunk[0] << 16) | ((chunk[1] ?? 0) << 8) | (chunk[2] ?? 0);
    for (let j = 0; j <= chunk.length; j++) out += B64[(n >> (18 - 6 * j)) & 63];
  }
  return out;
}

// Strict inverse of b64: URL alphabet, no padding, zero trailing bits.
function unb64(text) {
  if (text.length % 4 === 1) throw new Error('bad base64');
  const out = [];
  let acc = 0;
  let bits = 0;
  for (const c of text) {
    const v = B64.indexOf(c);
    if (v < 0) throw new Error('bad base64');
    acc = (acc << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push((acc >> bits) & 255);
      acc &= (1 << bits) - 1;
    }
  }
  if (acc !== 0) throw new Error('bad base64');
  return Uint8Array.from(out);
}

function concat(...parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

// No early exit on the first differing byte.
function same(a, b) {
  let diff = a.length ^ b.length;
  for (let i = 0; i < Math.max(a.length, b.length); i++) diff |= (a.charCodeAt(i) || 0) ^ (b.charCodeAt(i) || 0);
  return diff === 0;
}

function checkPublic(pub) {
  if (pub.length !== 65 || pub[0] !== 4) throw new Error('bad public key');
}

// A backend is { kind, sha256, generate, importPrivate, ecdh, hkdf, aesKey,
// seal, open }; a private key is opaque to the caller, a public one 65 bytes.
function webBackend(subtle) {
  const curve = { name: 'ECDH', namedCurve: 'P-256' };
  return {
    kind: 'webcrypto',
    sha256: async (bytes) => new Uint8Array(await subtle.digest('SHA-256', bytes)),
    async generate() {
      const pair = await subtle.generateKey(curve, false, ['deriveBits']);
      return { secret: pair.privateKey, pub: new Uint8Array(await subtle.exportKey('raw', pair.publicKey)) };
    },
    async importPrivate(scalar, pub) {
      const jwk = { kty: 'EC', crv: 'P-256', d: b64(scalar), x: b64(pub.subarray(1, 33)), y: b64(pub.subarray(33, 65)) };
      return { secret: await subtle.importKey('jwk', jwk, curve, false, ['deriveBits']), pub };
    },
    async ecdh(secret, pub) {
      checkPublic(pub);
      const key = await subtle.importKey('raw', pub, curve, false, []);
      return new Uint8Array(await subtle.deriveBits({ name: 'ECDH', public: key }, secret, 256));
    },
    async hkdf(ikm, salt, info) {
      const base = await subtle.importKey('raw', ikm, 'HKDF', false, ['deriveBits']);
      return new Uint8Array(await subtle.deriveBits({ name: 'HKDF', hash: 'SHA-256', salt, info }, base, 512));
    },
    aesKey: (raw) => subtle.importKey('raw', raw, 'AES-GCM', false, ['encrypt', 'decrypt']),
    seal: async (key, iv, aad, data) =>
      new Uint8Array(await subtle.encrypt({ name: 'AES-GCM', iv, additionalData: aad, tagLength: 128 }, key, data)),
    open: async (key, iv, aad, data) =>
      new Uint8Array(await subtle.decrypt({ name: 'AES-GCM', iv, additionalData: aad, tagLength: 128 }, key, data)),
  };
}

function nobleBackend(n) {
  return {
    kind: 'noble',
    sha256: async (bytes) => n.sha256(bytes),
    async generate() {
      const secret = n.p256.utils.randomSecretKey();
      return { secret, pub: n.p256.getPublicKey(secret, false) };
    },
    importPrivate: async (scalar, pub) => ({ secret: scalar, pub }),
    async ecdh(secret, pub) {
      checkPublic(pub);
      return n.p256.getSharedSecret(secret, pub, true).slice(1); // throws off the curve
    },
    hkdf: async (ikm, salt, info) => n.hkdf(n.sha256, ikm, salt, info, 64),
    aesKey: async (raw) => raw,
    seal: async (key, iv, aad, data) => n.gcm(key, iv, aad).encrypt(data),
    open: async (key, iv, aad, data) => n.gcm(key, iv, aad).decrypt(data),
  };
}

// noble.js is only fetched by a page that has no crypto.subtle.
function loadNoble() {
  return new Promise((resolve, reject) => {
    const el = document.createElement('script');
    el.src = '/noble.js';
    el.addEventListener('load', () => (globalThis.h0xNoble ? resolve(globalThis.h0xNoble) : reject(new Error('crypto script is empty'))));
    el.addEventListener('error', () => reject(new Error('could not load the crypto script')));
    document.head.append(el);
  });
}

let backendOnce = null;
function backend() {
  if (!backendOnce) {
    const first = globalThis.crypto && globalThis.crypto.subtle ? Promise.resolve(webBackend(crypto.subtle)) : loadNoble().then(nobleBackend);
    backendOnce = first.catch((e) => { backendOnce = null; throw e; });
  }
  return backendOnce;
}

// The device key D: made once per origin and kept in IndexedDB. With WebCrypto
// it is a non-extractable CryptoKey; noble has no such thing, so the scalar is
// stored (the origin's own storage, as the session token is).
function database() {
  return new Promise((resolve, reject) => {
    if (typeof indexedDB === 'undefined') return reject(new Error('this browser cannot keep a device key'));
    const req = indexedDB.open('h0x', 1);
    req.onupgradeneeded = () => req.result.createObjectStore('keys');
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(new Error('this browser cannot keep a device key'));
  });
}

async function keyStore(mode, run) {
  const db = await database();
  try {
    return await new Promise((resolve, reject) => {
      const tx = db.transaction('keys', mode);
      const req = run(tx.objectStore('keys'));
      tx.oncomplete = () => resolve(req.result);
      tx.onerror = () => reject(new Error('could not store the device key'));
      tx.onabort = tx.onerror;
    });
  } finally {
    db.close();
  }
}

async function device(be) {
  const rec = await keyStore('readonly', (s) => s.get('device'));
  if (rec && rec.kind === be.kind && rec.secret && rec.pub) return rec;
  const made = await be.generate();
  const fresh = { kind: be.kind, secret: made.secret, pub: made.pub };
  await keyStore('readwrite', (s) => s.put(fresh, 'device'));
  return fresh;
}

function clientHello(dPub, ecPub, nonce) {
  return `{"v":1,"d":"${b64(dPub)}","e":"${b64(ecPub)}","n":"${b64(nonce)}"}`;
}

function direction(dir) {
  return Uint8Array.of(dir >>> 24, (dir >>> 16) & 255, (dir >>> 8) & 255, dir & 255);
}

function frameNonce(dir, ctr) {
  const nonce = new Uint8Array(12);
  nonce.set(direction(dir));
  new DataView(nonce.buffer).setBigUint64(4, ctr, false);
  return nonce;
}

async function lane(be, raw, dir) {
  return { key: await be.aesKey(raw), dir, ctr: 0n };
}

// The next frame of a lane (`op` is 'seal' or 'open'); a failure does not advance it.
async function frame(be, lane, op, data) {
  if (lane.ctr >= MAX_CTR) throw new Error('counter exhausted');
  const out = await be[op](lane.key, frameNonce(lane.dir, lane.ctr), concat(PROTO, direction(lane.dir)), data);
  lane.ctr += 1n;
  return out;
}

// Our half of the handshake: the hello to send first.
async function begin(be) {
  const dev = await device(be);
  const eph = await be.generate();
  const nonce = crypto.getRandomValues(new Uint8Array(32));
  const text = clientHello(dev.pub, eph.pub, nonce);
  return { be, dev, eph, nonce, text, hello: enc.encode(text) };
}

// The server hello (its exact text) against the pinned fingerprint `want`:
// the two lanes. A wrong fingerprint is `block`ed, never retried.
async function establish(hs, text, want) {
  const { be } = hs;
  let hello;
  try { hello = JSON.parse(text); } catch { throw new Error('bad server hello'); }
  if (!hello || hello.v !== 1 || [hello.s, hello.e, hello.n].some((f) => typeof f !== 'string')) throw new Error('bad server hello');
  const sPub = unb64(hello.s);
  const esPub = unb64(hello.e);
  const sNonce = unb64(hello.n);
  if (sPub.length !== 65 || esPub.length !== 65 || sNonce.length !== 32) throw new Error('bad server hello');
  if (!same(b64(await be.sha256(sPub)), want)) {
    const e = new Error('identity mismatch');
    e.block = true;
    throw e;
  }
  const ikm = concat(await be.ecdh(hs.eph.secret, esPub), await be.ecdh(hs.eph.secret, sPub), await be.ecdh(hs.dev.secret, esPub));
  const info = concat(PROTO, await be.sha256(concat(hs.hello, enc.encode(text))));
  const okm = await be.hkdf(ikm, concat(hs.nonce, sNonce), info);
  return { be, out: await lane(be, okm.slice(0, 32), C2S), inn: await lane(be, okm.slice(32, 64), S2C) };
}

// ---- connection ----

// Run `fn` after everything queued before it, while `sock` is still the
// current socket; a failure ends the socket.
function step(sock, fn, closing = false) {
  tail = tail.then(async () => {
    if (ws !== sock || (sock.dead && !closing)) return;
    try { await fn(); } catch (e) { stop(sock, e); }
  });
}

function stop(sock, e) {
  chan = null;
  sock.dead = true; // frames already queued are not looked at
  if (e.block) {
    block('This is not the computer this device was paired with, so nothing was sent. If you replaced or reinstalled that computer, forget it below and scan its QR code again; otherwise someone may be impersonating it.');
  } else {
    sock.refused = `secure connection failed (${e.message})`;
    toast(`Secure connection failed: ${e.message}`, 'bad', null, 0);
  }
  sock.close();
}

function connect() {
  clearTimeout(retryTimer);
  if (ws || blocked) return;
  if (!session && !pairCode) return needPair('This device is not paired yet.');
  if (linkFp && pinned && linkFp !== pinned) {
    return block('This link is for a different computer than the one this device is paired with, so it was not opened. If that is not what you expected, someone may be impersonating it. To switch computers, forget this one below.');
  }
  const want = linkFp || pinned;
  if (!want) return needPair('Scan the QR code again: this page needs the fingerprint that is in the QR code link.');
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const sock = new WebSocket(`${proto}//${location.host}/ws`);
  sock.binaryType = 'arraybuffer';
  ws = sock;
  chan = null;
  let hs = null;
  let pairing = false;
  sock.refused = '';
  sock.addEventListener('open', () => {
    lastHeard = Date.now();
    clearInterval(pingTimer);
    pingTimer = setInterval(() => {
      if (Date.now() - lastHeard > DEAD_MS) sock.close();
      else send('ping', null);
    }, PING_MS);
    step(sock, async () => {
      hs = await begin(await backend());
      sock.send(hs.text);
    });
  });
  sock.addEventListener('message', (event) => step(sock, async () => {
    if (!chan) {
      if (!hs || typeof event.data !== 'string') throw new Error('unexpected frame during the handshake');
      chan = await establish(hs, event.data, want);
      usedFp = want;
      lastHeard = Date.now();
      if (pairCode) {
        pairing = true;
        const label = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || 'browser';
        send('pair', { token: pairCode, label: `${label} browser` });
        pairCode = null; // works once, either way
      } else {
        send('auth', { session });
      }
      return;
    }
    if (typeof event.data === 'string') throw new Error('unencrypted frame');
    const text = dec.decode(await frame(chan.be, chan.inn, 'open', new Uint8Array(event.data)));
    lastHeard = Date.now();
    let msg;
    try { msg = JSON.parse(text); } catch { return; }
    if (!ready && msg.name === 'error') sock.refused = String((msg.data && msg.data.message) || 'refused');
    receive(msg.name, msg.data || {});
  }));
  sock.addEventListener('close', () => step(sock, async () => {
    clearInterval(pingTimer);
    ws = null;
    chan = null;
    const was = ready;
    const refused = sock.refused;
    ready = false;
    for (const [, done] of pending) done({ kind: 'failed', message: 'connection lost' });
    pending.clear();
    if (blocked) return;
    if (!was && pairing) return needPair(refused ? `Pairing failed: ${refused}` : 'Pairing failed.');
    if (!was && refused && /pair/i.test(refused)) return needPair(`This device is no longer paired (${refused}).`);
    if (pairCode) return connect(); // a code was typed while this one closed
    if (!session) return needPair('');
    const wait = Math.min(30, 2 ** Math.min(retries, 5));
    retries += 1;
    pill(`Reconnecting in ${wait}s`, 'wait');
    retryTimer = setTimeout(connect, wait * 1000);
  }, true));
}

function send(name, data) {
  const sock = ws;
  const sealed = chan;
  if (!sealed || !sock || sock.readyState !== WebSocket.OPEN) return;
  const plain = enc.encode(JSON.stringify({ name, data }));
  step(sock, async () => sock.send(await frame(sealed.be, sealed.out, 'seal', plain)));
}

// A command; `done` gets the reply data once.
function command(cmd, args, done = () => {}) {
  if (!ready) return toast('Not connected yet.', 'bad');
  const id = nextId++;
  pending.set(id, done);
  send('command', { cmd, id, ...args });
}

function receive(name, data) {
  switch (name) {
    case 'paired':
      session = data.session;
      store.set(session);
      pinned = usedFp; // this computer, from now on
      pin.set(pinned);
      toast(`Paired as ${data.device || 'this device'}.`, 'good');
      // falls through
    case 'hello':
      tunnel(data);
      ready = true;
      retries = 0;
      setLocked(false);
      if (current) { show('agent'); read(); } else show('list');
      break;
    case 'snapshot':
      tunnel(data);
      setLocked(false);
      render(Array.isArray(data.lines) ? data.lines : []);
      break;
    case 'reply': {
      const done = pending.get(data.id);
      pending.delete(data.id);
      answer(data, done || (() => {}));
      break;
    }
    case 'notice':
      notice(data);
      break;
    case 'error':
      if (ready) toast(String(data.message || 'error'), 'bad');
      break;
    default: // pong, heartbeat
  }
}

// What every reply means to a person; `done` gets the text of a success.
function answer(r, done) {
  switch (r.kind) {
    case 'done': done(r); break;
    case 'needs_confirm':
      ask(String(r.what || 'Run this?'), () => command('confirm', { token: r.token }, (c) => answer(c, done)));
      break;
    case 'locked': setLocked(true); done(r); break;
    case 'rate_limited': toast(`Too many commands. Try again in ${r.retry_secs} s.`, 'bad'); done(r); break;
    case 'not_paired': needPair('This device was unpaired on the computer.'); break;
    default: toast(String(r.message || 'failed'), 'bad'); done(r);
  }
}

// ---- confirm dialog ----

let onYes = null;
function ask(what, yes) {
  const box = $('confirm');
  if (box.open) box.close(); // a newer question replaces an unanswered one
  onYes = yes;
  $('confirm-what').textContent = what;
  box.showModal();
  $('confirm-no').focus();
}
function cancel() {
  onYes = null;
  $('confirm').close();
  toast('Cancelled.');
}
$('confirm-yes').addEventListener('click', () => {
  const yes = onYes;
  onYes = null;
  $('confirm').close();
  if (yes) yes();
});
$('confirm-no').addEventListener('click', cancel);
$('confirm').addEventListener('cancel', (e) => { e.preventDefault(); cancel(); });

// ---- agents ----

// One `/agents` line: "<address>[ <name>] · <agent> · <folder> · <status>".
function parse(line) {
  const parts = line.split(' · ');
  if (parts.length < 4) return null;
  const head = parts[0];
  const space = head.indexOf(' ');
  const address = space < 0 ? head : head.slice(0, space);
  const name = space < 0 ? '' : head.slice(space + 1);
  const status = parts[parts.length - 1];
  const folder = parts.slice(2, -1).join(' · ');
  return { address, name, agent: parts[1], folder, status, kind: kindOf(status) };
}

function kindOf(status) {
  if (status === 'blocked') return 'needs';
  if (status === 'working' || status === 'error' || status === 'done') return status;
  if (/^(exit|signal|ended)/.test(status)) return 'ended';
  return 'idle';
}

const ORDER = { needs: 0, error: 1, working: 2, done: 3, idle: 4, ended: 5 };
const LABEL = { needs: 'Needs you', error: 'Error', working: 'Working', done: 'Done' };

function render(rows) {
  agents = rows.map(parse).filter(Boolean)
    .sort((a, b) => ORDER[a.kind] - ORDER[b.kind] || a.address.localeCompare(b.address, undefined, { numeric: true }));
  const list = $('agents');
  list.replaceChildren(...agents.map((a) => {
    const li = document.createElement('li');
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = `agent ${a.kind}`;
    const dot = document.createElement('span');
    dot.className = `dot ${a.kind}`;
    dot.setAttribute('aria-hidden', 'true');
    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = a.name ? `${a.name} · ${a.agent}` : `${a.agent} (${a.address})`;
    const state = document.createElement('span');
    state.className = 'state';
    state.textContent = LABEL[a.kind] || a.status;
    const folder = document.createElement('span');
    folder.className = 'folder';
    folder.textContent = a.folder;
    btn.append(dot, name, state, folder);
    btn.setAttribute('aria-label', `${name.textContent}, ${state.textContent}, in ${a.folder}`);
    btn.addEventListener('click', () => openAgent(a.address));
    li.append(btn);
    return li;
  }));
  $('empty').hidden = agents.length > 0;
  if (current) meta();
}

function refresh() {
  if (ready) send('snapshot', null);
  else connect();
}

let soon = 0;
function refreshSoon() {
  clearTimeout(soon);
  soon = setTimeout(refresh, 400);
}

function notice(n) {
  const who = n.agent || n.terminal;
  const text = {
    needs_you: `${who} needs you`,
    done: `${who} is done`,
    error: `${who} hit an error`,
    finished: `${who} finished`,
  }[n.kind] || `${who}: ${n.kind}`;
  const body = n.excerpt ? `${text}: ${n.excerpt}` : text;
  const needs = n.kind === 'needs_you';
  toast(body, needs ? 'needs' : n.kind === 'error' ? 'bad' : 'good', n.terminal, needs ? 0 : 8000);
  if (window.Notification && Notification.permission === 'granted' && document.hidden) {
    try { new Notification(`HyperOx: ${text}`, { body: String(n.excerpt || ''), tag: String(n.terminal) }); } catch { /* needs a service worker here */ }
  }
  refreshSoon();
  if (current === n.terminal) read();
}

// ---- one agent ----

function meta() {
  const a = agents.find((x) => x.address === current);
  $('agent-title').textContent = a ? (a.name || a.agent) : current;
  $('agent-meta').textContent = a ? `${a.agent} · ${a.folder} · ${LABEL[a.kind] || a.status}` : current;
}

function openAgent(address) {
  if (current !== address) history.pushState({ agent: address }, '');
  current = address;
  lines = FIRST_LINES;
  $('out').textContent = 'Loading…';
  meta();
  show('agent');
  read();
}

function read() {
  const address = current;
  $('more').hidden = lines >= MAX_LINES;
  command('read', { terminal: address, lines }, (r) => {
    if (current !== address || r.kind !== 'done') return;
    const out = $('out');
    out.textContent = r.text || '(no output yet)';
    out.scrollTop = out.scrollHeight;
  });
}

function closeAgent() {
  current = null;
  show(ready || session ? 'list' : 'pair');
  refresh();
}

function after(r) {
  if (r.kind !== 'done') return;
  if (r.text) toast(String(r.text), 'good', null, 4000);
  setTimeout(() => { if (current) read(); }, 600);
}

$('back').addEventListener('click', () => history.back());
window.addEventListener('popstate', () => { if (current) closeAgent(); });
$('more').addEventListener('click', () => { lines = Math.min(MAX_LINES, lines + 60); read(); });
$('reread').addEventListener('click', read);
$('send-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const input = $('send-text');
  if (!input.value || !current) return;
  const text = input.value;
  command('send', { terminal: current, text }, (r) => { if (r.kind === 'done') input.value = ''; after(r); });
});
for (const b of document.querySelectorAll('[data-key]')) {
  b.addEventListener('click', () => command('keys', { terminal: current, keys: [b.dataset.key] }, after));
}
for (const b of document.querySelectorAll('[data-cmd]')) {
  b.addEventListener('click', () => command(b.dataset.cmd, { terminal: current }, after));
}

// ---- list, run, lock, alerts ----

$('refresh').addEventListener('click', refresh);

$('run-open').addEventListener('click', () => { $('run').showModal(); $('run-folder').focus(); });
$('run-cancel').addEventListener('click', () => $('run').close());
$('run-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const folder = $('run-folder').value.trim();
  const cmd = $('run-command').value.trim();
  if (!folder || !cmd) return;
  $('run').close();
  // The server always asks to confirm a run; the dialog shows what it will do.
  command('run', { folder, command: cmd }, (r) => {
    if (r.kind === 'done') { toast(String(r.text || 'Started.'), 'good'); refreshSoon(); }
  });
});

$('lock').addEventListener('click', () => {
  ask('Lock remote control? Nothing can be run from any paired device until you unlock it on the computer with `h0x-core bridge unlock`.',
    () => command('lock', {}, (r) => { if (r.kind === 'done') setLocked(true); }));
});

function alertsButton() {
  const can = window.Notification && window.isSecureContext && Notification.permission === 'default';
  $('alerts').hidden = !can;
}
$('alerts').addEventListener('click', () => {
  Notification.requestPermission().then(alertsButton, alertsButton);
});

// Pull down at the top of the list to refresh.
let pullFrom = -1;
const pull = $('pull');
$('list').addEventListener('touchstart', (e) => { pullFrom = window.scrollY === 0 ? e.touches[0].clientY : -1; }, { passive: true });
$('list').addEventListener('touchmove', (e) => {
  if (pullFrom >= 0) pull.classList.toggle('show', e.touches[0].clientY - pullFrom > 70);
}, { passive: true });
$('list').addEventListener('touchend', () => {
  if (pull.classList.contains('show')) refresh();
  pull.classList.remove('show');
  pullFrom = -1;
});

// $('pair-form'): a code typed from `h0x-core bridge pair --transport web`.
$('forget').addEventListener('click', () => {
  store.drop();
  pin.drop();
  session = null;
  pinned = null;
  blocked = false;
  if (ws) ws.close(); // the close finds no session and shows the pair screen
  else connect();
});

$('pair-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const code = cleanCode($('pair-code').value);
  if (!code) return;
  $('pair-code').value = '';
  pairCode = code;
  pill('Pairing', 'wait');
  if (ws) ws.close();
  else connect();
});

// Come back fast after the phone sleeps or the network returns.
const wake = () => { if (!ws && session && !document.hidden) { retries = 0; connect(); } };
document.addEventListener('visibilitychange', wake);
window.addEventListener('online', wake);

const host = location.hostname.toLowerCase();
const local = /^127(\.\d{1,3}){3}$/.test(host) || host === 'localhost' || host === 'localhost.' || host === '[::1]';
$('insecure').hidden = location.protocol !== 'http:' || local;
alertsButton();
pill('Connecting', 'wait');
connect();
