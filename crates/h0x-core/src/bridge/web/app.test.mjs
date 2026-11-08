// The web panel's logic, run by `node --test` against a fake page (no DOM
// library, no dependency): app.js loads unchanged into a vm context with
// stand-ins for the browser objects it touches. Run from assets.rs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const SOURCE = readFileSync(new URL('./app.js', import.meta.url), 'utf8');
const PAGE = readFileSync(new URL('./page.html', import.meta.url), 'utf8');

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
// `storage: 'throws'` is a private-mode localStorage.
function load({ href = 'http://192.168.1.20:8377/', stored = null, storage = 'ok', permission = 'default', secure = false, hidden = false } = {}) {
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

  const saved = new Map(stored ? [['h0x-session', stored]] : []);
  const nope = () => { throw new Error('SecurityError'); };
  const localStorage = storage === 'throws'
    ? { getItem: nope, setItem: nope, removeItem: nope }
    : { getItem: (k) => saved.get(k) ?? null, setItem: (k, v) => saved.set(k, String(v)), removeItem: (k) => saved.delete(k) };

  const w = { now: 0, sockets: [], timers: new Map(), history: [], notes: [], asked: 0, saved, keys, cmds, $ };
  let timerId = 0;
  const later = (repeat) => (fn, ms) => { timerId += 1; w.timers.set(timerId, { fn, ms, repeat }); return timerId; };
  const clear = (id) => w.timers.delete(id);

  class FakeSocket {
    static OPEN = 1;
    constructor(target) { this.url = target; this.readyState = 0; this.sent = []; this.on = {}; w.sockets.push(this); }
    addEventListener(type, fn) { (this.on[type] ||= []).push(fn); }
    emit(type, event) { for (const fn of this.on[type] || []) fn(event); }
    send(text) { this.sent.push(JSON.parse(text)); }
    open() { this.readyState = 1; this.emit('open', {}); }
    msg(name, data) { this.emit('message', { data: JSON.stringify({ name, data }) }); }
    close() { if (this.readyState === 3) return; this.readyState = 3; this.emit('close', {}); }
  }
  function Notification(title, opts) { w.notes.push(JSON.parse(JSON.stringify({ title, opts }))); }
  Notification.permission = permission;
  Notification.requestPermission = () => { w.asked += 1; return Promise.resolve(Notification.permission); };

  const ctx = vm.createContext({
    URLSearchParams,
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

// A page with an authenticated socket.
function connected(opts = {}) {
  const w = load({ stored: 'S1', ...opts });
  w.ws().open();
  w.ws().msg('hello', null);
  return w;
}

test('a pair code in the address is stripped, sent once as pair, and the session is used on reload', () => {
  const w = load({ href: 'http://192.168.1.20:8377/?v=1#pair=ABCD2345' });
  assert.deepEqual(w.history, [['replace', null, '', '/?v=1']]);
  assert.equal(w.ws().url, 'ws://192.168.1.20:8377/ws');
  w.ws().open();
  assert.deepEqual(w.ws().sent, [{ name: 'pair', data: { token: 'ABCD2345', label: 'Linux browser' } }]);
  w.ws().msg('paired', { session: 'S9', device: 'Linux browser' });
  assert.equal(w.saved.get('h0x-session'), 'S9');
  assert.equal(w.$('list').hidden, false);
  w.ws().close();
  assert.equal(w.reconnect(), 1000);
  w.ws().open();
  assert.deepEqual(w.ws().sent, [{ name: 'auth', data: { session: 'S9' } }], 'the code is not sent again');

  const reload = load({ stored: 'S9' });
  assert.deepEqual(reload.history, [], 'nothing to strip');
  reload.ws().open();
  assert.deepEqual(reload.ws().sent, [{ name: 'auth', data: { session: 'S9' } }]);
});

test('the pair fragment parses empty, padded, very long and mixed fragments', () => {
  const long = 'A'.repeat(10000);
  for (const [hash, token] of [
    ['#pair=AB&x=1', 'AB'],
    ['#x=1&pair=AB', 'AB'],
    ['#pair=%20AB%20', 'AB'],
    ['#pair=abcd2345', 'ABCD2345'],
    ['#pair=ab%20cd', 'AB'],
    ['#pair=ab%23cd', 'AB'],
    ['#pair=%20%20', null],
    [`#pair=${long}`, long],
  ]) {
    const w = load({ href: `http://192.168.1.20:8377/${hash}` });
    assert.deepEqual(w.history, [['replace', null, '', '/']], hash);
    if (token === null) { assert.equal(w.sockets.length, 0, hash); continue; }
    w.ws().open();
    assert.deepEqual(w.ws().sent[0].data.token, token, hash);
  }
  for (const hash of ['#pair=', '#pair', '#x=1', '#']) {
    const w = load({ href: `http://192.168.1.20:8377/${hash}` });
    assert.equal(w.sockets.length, 0, `${hash}: no code, no socket`);
    assert.equal(w.$('pair').hidden, false, hash);
    assert.equal(w.$('pair-why').textContent, 'This device is not paired yet.', hash);
  }
});

test('a refused pairing or a forgotten session brings the pair screen back', () => {
  const w = load({ href: 'http://192.168.1.20:8377/#pair=BAD' });
  w.ws().open();
  w.ws().msg('error', { message: 'pairing code expired' });
  w.ws().close();
  assert.equal(w.$('pair').hidden, false);
  assert.equal(w.$('pair-why').textContent, 'Pairing failed: pairing code expired');
  assert.equal(w.timers.size, 0, 'no retry with a used code');

  const silent = load({ href: 'http://192.168.1.20:8377/#pair=BAD' });
  silent.ws().open();
  silent.ws().close();
  assert.equal(silent.$('pair-why').textContent, 'Pairing failed.');

  const forgot = load({ stored: 'S1' });
  forgot.ws().open();
  forgot.ws().msg('error', { message: 'unknown or revoked session; pair again' });
  forgot.ws().close();
  assert.equal(forgot.$('pair').hidden, false);
  assert.equal(forgot.saved.has('h0x-session'), false, 'the dead session is dropped');

  const busy = load({ stored: 'S1' });
  busy.ws().open();
  busy.ws().msg('error', { message: 'server busy' });
  busy.ws().close();
  assert.equal(busy.$('pair').hidden, true, 'other refusals retry');
  assert.equal(busy.reconnect(), 1000);
});

test('a typed code is trimmed and an empty one is ignored', () => {
  const w = load();
  assert.equal(w.sockets.length, 0);
  w.$('pair-code').value = '   ';
  w.$('pair-form').fire('submit');
  assert.equal(w.sockets.length, 0);
  w.$('pair-code').value = '  abcd2345 ';
  w.$('pair-form').fire('submit');
  w.ws().open();
  assert.equal(w.ws().sent[0].data.token, 'ABCD2345');
  assert.equal(w.$('pair-code').value, '');
});

test('a typed code is cut like one from the link', () => {
  for (const typed of ['abcd2345#x', 'ABCD2345&pair=1', 'abcd2345 extra', '\tabcd2345\n']) {
    const w = load();
    w.$('pair-code').value = typed;
    w.$('pair-form').fire('submit');
    w.ws().open();
    assert.equal(w.ws().sent[0].data.token, 'ABCD2345', JSON.stringify(typed));
  }
  const none = load();
  none.$('pair-code').value = '#x';
  none.$('pair-form').fire('submit');
  assert.equal(none.sockets.length, 0, 'nothing left of the code');
});

test('a localStorage that throws still pairs and reconnects within the tab', () => {
  const w = load({ href: 'http://192.168.1.20:8377/#pair=ABCD2345', storage: 'throws' });
  w.ws().open();
  w.ws().msg('paired', { session: 'S9' });
  assert.equal(w.$('list').hidden, false);
  w.ws().close();
  assert.equal(w.reconnect(), 1000);
  w.ws().open();
  assert.deepEqual(w.ws().sent, [{ name: 'auth', data: { session: 'S9' } }]);
  w.ws().msg('error', { message: 'unknown or revoked session; pair again' });
  w.ws().close();
  assert.equal(w.$('pair').hidden, false, 'dropping the session does not throw either');
});

test('reconnects back off 1, 2, 4, 8, 16, 30, 30 s and reset once heard', () => {
  const w = load({ stored: 'S1' });
  const waits = [];
  for (let i = 0; i < 7; i += 1) {
    w.ws().close();
    waits.push(w.reconnect());
  }
  assert.deepEqual(waits, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
  w.ws().open();
  w.ws().msg('hello', null);
  w.ws().close();
  assert.equal(w.$('pill').textContent, 'Reconnecting in 1s');
  assert.equal(w.reconnect(), 1000);
});

test('pings every 25 s and gives the socket up after 50 s of silence', () => {
  const w = connected();
  const pings = [...w.timers.values()].filter((t) => t.repeat);
  assert.deepEqual(pings.map((t) => t.ms), [25000]);
  const tick = pings[0].fn;
  w.now = 25000;
  tick();
  assert.deepEqual(w.ws().sent.at(-1), { name: 'ping', data: null });
  w.ws().msg('pong', null); // heard at 25 s
  w.now = 75000; // exactly 50 s: still alive
  tick();
  assert.equal(w.ws().readyState, 1);
  w.now = 75001;
  tick();
  assert.equal(w.ws().readyState, 3);
  assert.equal([...w.timers.values()].some((t) => t.repeat), false, 'the ping stops with the socket');
});

test('the agents list puts needs-you first, then error, working, done, idle, ended', () => {
  const w = connected();
  w.ws().msg('snapshot', {
    lines: [
      't7 · claude · ~/a · idle',
      't10 · codex · ~/b · blocked',
      't2 bob · codex · C:\\Users\\me\\proj · blocked',
      't3 · claude · ~/x · y · working',
      't4 · claude · ~/d · exit 0',
      't5 · claude · ~/e · error',
      't6 · claude · ~/f · done',
      'garbage',
      't8 · too · short',
      '',
    ],
  });
  assert.deepEqual(w.value('agents.map((a) => a.address)'), ['t2', 't10', 't5', 't3', 't6', 't7', 't4']);
  const rows = w.$('agents').children.map((li) => li.children[0].children.map((s) => s.textContent));
  assert.deepEqual(rows[0], ['', 'bob · codex', 'Needs you', 'C:\\Users\\me\\proj']);
  assert.equal(rows[3][3], '~/x · y', 'a folder may contain the separator');
  assert.equal(w.$('empty').hidden, true);
  for (const lines of [[], undefined, 'not a list']) {
    w.ws().msg('snapshot', { lines });
    assert.equal(w.$('agents').children.length, 0);
    assert.equal(w.$('empty').hidden, false);
  }
});

test('the agent view reads 80 lines, then 60 more up to 200', () => {
  const w = connected();
  w.ws().msg('snapshot', { lines: ['t2 · claude · ~/a · blocked'] });
  w.$('agents').children[0].children[0].fire('click');
  assert.equal(w.$('agent').hidden, false);
  const reads = () => w.commands().filter((c) => c.cmd === 'read').map((c) => c.lines);
  assert.deepEqual(reads(), [80]);
  assert.equal(w.$('more').hidden, false);
  w.$('more').fire('click');
  assert.equal(w.$('more').hidden, false);
  w.$('more').fire('click');
  assert.equal(w.$('more').hidden, true);
  w.$('more').fire('click');
  assert.deepEqual(reads(), [80, 140, 200, 200]);
  const last = w.commands().at(-1);
  w.ws().msg('reply', { id: last.id, kind: 'done', text: '' });
  assert.equal(w.$('out').textContent, '(no output yet)');
});

test('quick keys and actions go to the open agent; nothing goes before the socket is ready', () => {
  const early = load({ stored: 'S1' });
  early.ws().open();
  early.keys[0].fire('click');
  assert.deepEqual(early.commands(), []);
  assert.deepEqual(early.toasts(), ['Not connected yet.']);

  const w = connected();
  w.run("openAgent('t2')");
  for (const b of w.keys) b.fire('click');
  for (const b of w.cmds) b.fire('click');
  const sent = w.commands().filter((c) => c.cmd !== 'read').map(({ id, ...c }) => c);
  assert.deepEqual(sent, [
    ...['enter', 'esc', 'ctrl+c', 'up', 'down', 'y', 'n'].map((k) => ({ cmd: 'keys', terminal: 't2', keys: [k] })),
    ...['approve', 'deny', 'interrupt', 'kill'].map((cmd) => ({ cmd, terminal: 't2' })),
  ]);
});

test('a command needing confirmation asks first; cancel sends nothing', () => {
  const w = connected();
  w.run("openAgent('t2')");
  w.cmds.find((b) => b.dataset.cmd === 'kill').fire('click');
  const kill = w.commands().at(-1);
  w.ws().msg('reply', { id: kill.id, kind: 'needs_confirm', what: 'Kill t2?', token: 'tok' });
  assert.equal(w.$('confirm').open, true);
  assert.equal(w.$('confirm-what').textContent, 'Kill t2?');
  w.$('confirm-no').fire('click');
  assert.equal(w.$('confirm').open, false);
  assert.equal(w.commands().at(-1).id, kill.id, 'nothing sent on cancel');

  w.ws().msg('reply', { id: kill.id, kind: 'needs_confirm', what: 'Kill t2?', token: 'tok' });
  w.$('confirm-yes').fire('click');
  assert.equal(w.commands().at(-1).cmd, 'confirm');
  assert.equal(w.commands().at(-1).token, 'tok');
  w.$('confirm-yes').fire('click'); // a second press has nothing to confirm
  assert.equal(w.commands().filter((c) => c.cmd === 'confirm').length, 1);
});

test('run needs a folder and a command, trims them and keeps Windows paths', () => {
  const w = connected();
  w.$('run-open').fire('click');
  assert.equal(w.$('run').open, true);
  for (const [folder, command] of [['', 'claude'], ['~/p', '   '], ['  ', '  ']]) {
    w.$('run-folder').value = folder;
    w.$('run-command').value = command;
    w.$('run-form').fire('submit');
  }
  assert.deepEqual(w.commands(), []);
  assert.equal(w.$('run').open, true);
  w.$('run-folder').value = '  C:\\Users\\me\\my proj  ';
  w.$('run-command').value = ' claude ';
  w.$('run-form').fire('submit');
  assert.equal(w.$('run').open, false);
  const { id, ...run } = w.commands()[0];
  assert.deepEqual(run, { cmd: 'run', folder: 'C:\\Users\\me\\my proj', command: 'claude' });
});

test('lock asks, then locks the panel', () => {
  const w = connected();
  assert.equal(w.$('lock').hidden, false);
  w.$('lock').fire('click');
  assert.equal(w.$('confirm').open, true);
  w.$('confirm-yes').fire('click');
  const lock = w.commands().at(-1);
  assert.equal(lock.cmd, 'lock');
  w.ws().msg('reply', { id: lock.id, kind: 'done', text: 'locked' });
  assert.equal(w.$('locked').hidden, false);
  assert.equal(w.$('lock').disabled, true);
  assert.equal(w.$('pill').textContent, 'Locked');
});

test('notices toast; browser notifications wait for the button and permission', () => {
  const w = connected({ secure: true });
  assert.equal(w.$('alerts').hidden, false, 'offered on a secure page');
  w.ws().msg('notice', { terminal: 't2', agent: 'claude', kind: 'needs_you', excerpt: 'Allow?' });
  assert.deepEqual(w.toasts(), ['claude needs you: Allow?']);
  assert.equal(w.$('toasts').children[0].className, 'toast needs');
  assert.deepEqual(w.notes, [], 'no notification before permission');
  assert.equal(w.asked, 0);
  w.$('alerts').fire('click');
  assert.equal(w.asked, 1);
  for (let i = 0; i < 6; i += 1) w.ws().msg('notice', { terminal: `t${i}`, kind: 'done' });
  assert.equal(w.$('toasts').children.length, 4, 'at most four toasts');
  assert.equal(w.toasts().at(-1), 't5 is done');

  assert.equal(connected({ secure: false }).$('alerts').hidden, true, 'not offered over plain http');
  assert.equal(connected({ secure: true, permission: 'denied' }).$('alerts').hidden, true);

  const away = connected({ permission: 'granted', hidden: true });
  away.ws().msg('notice', { terminal: 't2', agent: 'claude', kind: 'error', excerpt: null });
  assert.deepEqual(away.notes, [{ title: 'HyperOx: claude hit an error', opts: { body: '', tag: 't2' } }]);
  const looking = connected({ permission: 'granted', hidden: false });
  looking.ws().msg('notice', { terminal: 't2', kind: 'done' });
  assert.deepEqual(looking.notes, [], 'no notification while the page is in view');
});

test('the plain-http banner shows off localhost only, the tunnel banner on request', () => {
  for (const [href, hidden] of [
    ['http://localhost:8377/', true],
    ['http://127.0.0.1:8377/', true],
    ['http://[::1]:8377/', true],
    ['http://127.1.2.3:8377/', true],
    ['http://LocalHost:8377/', true],
    ['http://localhost.:8377/', true],
    ['http://127.0.0.1.example.com:8377/', false],
    ['http://localhost.example.com:8377/', false],
    ['http://128.0.0.1:8377/', false],
    ['http://192.168.1.20:8377/', false],
    ['http://100.64.0.5:8377/', false],
    ['https://192.168.1.20:8377/', true],
    ['https://panel.example.trycloudflare.com/', true],
  ]) {
    assert.equal(load({ href, stored: 'S1' }).$('insecure').hidden, hidden, href);
  }
  assert.equal(load({ href: 'https://panel.example/', stored: 'S1' }).ws().url, 'wss://panel.example/ws');
  assert.equal(connected().$('tunnel').hidden, true);
  const tunnel = load({ stored: 'S1' });
  tunnel.ws().open();
  tunnel.ws().msg('hello', { tunnel: true });
  assert.equal(tunnel.$('tunnel').hidden, false);
});
