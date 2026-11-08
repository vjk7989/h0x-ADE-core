// A fake page and a fake server for the web panel tests (no DOM library, no
// dependency). app.js loads unchanged into a vm context with stand-ins for the
// browser objects it touches; the fake socket speaks the real encrypted
// protocol (ADR-064) from the server side with node:crypto only, so it checks
// app.js independently of the code under test. Used by app.test.mjs and
// e2e_client.test.mjs.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { createCipheriv, createDecipheriv, createECDH, createHash, hkdfSync, randomBytes, webcrypto } from 'node:crypto';

const read = (name) => readFileSync(new URL(`./${name}`, import.meta.url), 'utf8');
const SOURCE = read('app.js');
const PAGE = read('page.html');
const NOBLE = read('noble.js');

export const V = JSON.parse(read('e2e_vectors.json'));
export const unhex = (h) => Uint8Array.from(h.match(/../g) ?? [], (b) => parseInt(b, 16));
export const hex = (bytes) => Buffer.from(bytes).toString('hex');
const B64URL = (bytes) => Buffer.from(bytes).toString('base64url');
const LABEL = Buffer.from('h0x-bridge v1');
const nonceOf = (dir, ctr) => {
  const n = Buffer.alloc(12);
  n.writeUInt32BE(dir, 0);
  n.writeBigUInt64BE(ctr, 4);
  return n;
};
const aadOf = (dir) => Buffer.concat([LABEL, Buffer.from([0, 0, 0, dir])]);

// One connection's server half, from the client's exact hello text.
// `priv` is the server identity scalar (the vectors' S unless an impostor).
function server(helloText, priv) {
  const hello = JSON.parse(helloText);
  const [dPub, ecPub, cNonce] = [hello.d, hello.e, hello.n].map((f) => Buffer.from(f, 'base64url'));
  const s = createECDH('prime256v1');
  s.setPrivateKey(Buffer.from(priv, 'hex'));
  const es = createECDH('prime256v1');
  es.generateKeys();
  const sNonce = randomBytes(32);
  const text = `{"v":1,"s":"${B64URL(s.getPublicKey(null, 'uncompressed'))}","e":"${B64URL(es.getPublicKey(null, 'uncompressed'))}","n":"${B64URL(sNonce)}"}`;
  const ikm = Buffer.concat([es.computeSecret(ecPub), s.computeSecret(ecPub), es.computeSecret(dPub)]);
  const info = Buffer.concat([LABEL, createHash('sha256').update(helloText).update(text).digest()]);
  const okm = Buffer.from(hkdfSync('sha256', ikm, Buffer.concat([cNonce, sNonce]), info, 64));
  const lanes = {
    // The server opens what the client sealed (c2s, dir 1) and seals s2c (dir 2).
    in: { key: okm.subarray(0, 32), dir: 1, ctr: 0n },
    out: { key: okm.subarray(32, 64), dir: 2, ctr: 0n },
  };
  return {
    text,
    dPub,
    seal(plain) {
      const c = createCipheriv('aes-256-gcm', lanes.out.key, nonceOf(2, lanes.out.ctr), { authTagLength: 16 });
      c.setAAD(aadOf(2));
      lanes.out.ctr += 1n;
      return Buffer.concat([c.update(plain), c.final(), c.getAuthTag()]);
    },
    open(wire) {
      const d = createDecipheriv('aes-256-gcm', lanes.in.key, nonceOf(1, lanes.in.ctr), { authTagLength: 16 });
      d.setAAD(aadOf(1));
      d.setAuthTag(wire.subarray(wire.length - 16));
      const plain = Buffer.concat([d.update(wire.subarray(0, wire.length - 16)), d.final()]);
      lanes.in.ctr += 1n;
      return plain;
    },
  };
}

export const arrayBuffer = (buf) => buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.length);

// An in-memory IndexedDB, as much as app.js uses: open, one store, get, put.
function fakeIndexedDB(rows) {
  const later = (fn) => Promise.resolve().then(fn);
  return {
    open() {
      const req = {};
      later(() => {
        req.result = {
          createObjectStore() {},
          close() {},
          transaction() {
            const tx = {};
            const request = (result) => {
              const r = { result };
              later(() => later(() => tx.oncomplete && tx.oncomplete()));
              return r;
            };
            tx.objectStore = () => ({
              get: (k) => request(rows.get(k)),
              put: (v, k) => request(rows.set(k, v) && k),
              // Like IndexedDB: adding a key that exists aborts the transaction.
              add: (v, k) => {
                if (!rows.has(k)) return request(rows.set(k, v) && k);
                later(() => later(() => tx.onerror && tx.onerror()));
                return { result: undefined };
              },
            });
            return tx;
          },
        };
        if (req.onupgradeneeded) req.onupgradeneeded();
        req.onsuccess();
      });
      return req;
    },
  };
}

class El {
  constructor(id = '') {
    this.id = id;
    this.hidden = false;
    this.textContent = '';
    this.className = '';
    this.disabled = false;
    this.value = '';
    this.open = false;
    this.dataset = {};
    this.attrs = {};
    this.children = [];
    this.parent = null;
    this.on = {};
    this.scrollTop = 0;
    this.scrollHeight = 0;
    const classes = new Set();
    this.classList = {
      toggle: (c, on) => (on ? classes.add(c) : classes.delete(c)),
      contains: (c) => classes.has(c),
      remove: (c) => classes.delete(c),
    };
  }
  addEventListener(type, fn) { (this.on[type] ||= []).push(fn); }
  fire(type, event = {}) { for (const fn of this.on[type] || []) fn({ preventDefault() {}, ...event }); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  append(...kids) { for (const k of kids) { k.parent = this; this.children.push(k); } }
  replaceChildren(...kids) { this.children = []; this.append(...kids); }
  remove() {
    if (this.parent) this.parent.children = this.parent.children.filter((k) => k !== this);
    this.parent = null;
  }
  get firstElementChild() { return this.children[0]; }
  showModal() { this.open = true; }
  close() { this.open = false; }
  focus() {}
}

// `href` is where the page was served from; `stored` a session already kept;
// `pinned` the fingerprint a paired device kept (null: never paired; '' is kept as an empty value);
// `storage: 'throws'` is a private-mode localStorage; `subtle: false` a page
// without crypto.subtle (plain http), which fetches noble.js; `idb` shares
// the device key store between pages; `serverKey` is the server's identity.
export function load({
  href = 'http://192.168.1.20:8377/',
  stored = null,
  pinned = V.fp,
  storage = 'ok',
  permission = 'default',
  secure = false,
  hidden = false,
  subtle = true,
  idb = new Map(),
  serverKey = V.s_priv,
} = {}) {
  const url = new URL(href);
  const els = new Map();
  for (const m of PAGE.matchAll(/<[a-z]+\s[^>]*\bid="([^"]+)"[^>]*>/g)) {
    const el = new El(m[1]);
    el.hidden = /\shidden[\s>]/.test(m[0]);
    els.set(m[1], el);
  }
  const $ = (id) => { if (!els.has(id)) els.set(id, new El(id)); return els.get(id); };
  const marked = (attr) => [...PAGE.matchAll(new RegExp(`data-${attr}="([^"]+)"`, 'g'))].map((m) => {
    const el = new El();
    el.dataset[attr] = m[1];
    return el;
  });
  const keys = marked('key');
  const cmds = marked('cmd');

  const saved = new Map([...(stored ? [['h0x-session', stored]] : []), ...(pinned != null ? [['h0x-fp', pinned]] : [])]);
  const nope = () => { throw new Error('SecurityError'); };
  const localStorage = storage === 'throws'
    ? { getItem: nope, setItem: nope, removeItem: nope }
    : { getItem: (k) => saved.get(k) ?? null, setItem: (k, v) => saved.set(k, String(v)), removeItem: (k) => saved.delete(k) };

  const w = { now: 0, sockets: [], timers: new Map(), history: [], notes: [], asked: 0, saved, keys, cmds, idb, scripts: [], $ };
  let timerId = 0;
  const later = (repeat) => (fn, ms) => { timerId += 1; w.timers.set(timerId, { fn, ms, repeat }); return timerId; };
  const clear = (id) => w.timers.delete(id);

  class FakeSocket {
    static OPEN = 1;
    constructor(target) {
      this.url = target;
      this.readyState = 0;
      this.binaryType = 'blob';
      this.sent = []; // the app's messages, opened by the fake server
      this.hellos = []; // the plain text frames the client sent
      this.leaks = []; // text frames after the handshake: must stay empty
      this.srv = null;
      this.on = {};
      w.sockets.push(this);
    }
    addEventListener(type, fn) { (this.on[type] ||= []).push(fn); }
    emit(type, event) { for (const fn of this.on[type] || []) fn(event); }
    send(data) {
      if (typeof data === 'string') {
        if (this.srv) return this.leaks.push(data);
        this.hellos.push(data);
        this.srv = server(data, serverKey);
        // The server answers with its hello; the client may not have asked yet.
        return this.emit('message', { data: this.srv.text });
      }
      assert.ok(this.srv, 'a binary frame before the handshake');
      this.sent.push(JSON.parse(this.srv.open(Buffer.from(data)).toString('utf8')));
    }
    async open() { this.readyState = 1; this.emit('open', {}); await w.settle(); }
    // A sealed message from the server.
    async msg(name, data) {
      this.emit('message', { data: arrayBuffer(this.srv.seal(Buffer.from(JSON.stringify({ name, data })))) });
      await w.settle();
    }
    async close() {
      if (this.readyState === 3) return;
      this.readyState = 3;
      this.emit('close', {});
      await w.settle();
    }
  }
  function Notification(title, opts) { w.notes.push(JSON.parse(JSON.stringify({ title, opts }))); }
  Notification.permission = permission;
  Notification.requestPermission = () => { w.asked += 1; return Promise.resolve(Notification.permission); };

  const head = new El();
  head.append = (el) => {
    w.scripts.push(el.src);
    assert.equal(el.src, '/noble.js');
    vm.runInContext(NOBLE, ctx, { filename: 'noble.js' });
    el.fire('load');
  };
  const crypto = subtle ? webcrypto : { getRandomValues: (a) => webcrypto.getRandomValues(a) };
  const ctx = vm.createContext({
    URLSearchParams,
    TextEncoder,
    TextDecoder,
    crypto,
    indexedDB: fakeIndexedDB(idb),
    Date: { now: () => w.now },
    setTimeout: later(false),
    setInterval: later(true),
    clearTimeout: clear,
    clearInterval: clear,
    localStorage,
    location: { href, protocol: url.protocol, host: url.host, hostname: url.hostname, pathname: url.pathname, search: url.search, hash: url.hash },
    history: {
      replaceState: (...a) => w.history.push(['replace', ...a]),
      pushState: (...a) => w.history.push(['push', ...a]),
      back: () => w.history.push(['back']),
    },
    navigator: { platform: 'Linux' },
    WebSocket: FakeSocket,
    Notification,
    isSecureContext: secure,
    scrollY: 0,
    addEventListener() {},
    document: {
      hidden,
      head,
      getElementById: $,
      createElement: () => new El(),
      querySelectorAll: (sel) => (sel === '[data-key]' ? keys : sel === '[data-cmd]' ? cmds : []),
      addEventListener() {},
    },
  });
  ctx.window = ctx;
  vm.runInContext(SOURCE, ctx, { filename: 'app.js' });
  w.run = (code) => vm.runInContext(code, ctx);
  // A plain copy of a value made inside the page (deepEqual compares prototypes).
  w.value = (code) => JSON.parse(JSON.stringify(w.run(code)));
  w.ws = () => w.sockets.at(-1);
  // Waits until the page has nothing left queued on its sockets.
  w.settle = async () => {
    for (;;) {
      const queued = w.run('tail');
      await queued;
      await new Promise((resolve) => setImmediate(resolve));
      if (w.run('tail') === queued) return;
    }
  };
  // Runs the pending reconnect timer: its delay in ms.
  w.reconnect = () => {
    const connect = w.run('connect');
    const [id, t] = [...w.timers].find(([, t]) => t.fn === connect) || [];
    assert.ok(t, 'a reconnect is scheduled');
    w.timers.delete(id);
    t.fn();
    return t.ms;
  };
  w.toasts = () => $('toasts').children.map((t) => t.children[0].textContent);
  w.commands = () => w.ws().sent.filter((m) => m.name === 'command').map((m) => m.data);
  return w;
}
