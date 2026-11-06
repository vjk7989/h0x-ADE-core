// HyperOx web panel: pair from #pair= (or a typed code), keep the session,
// list the agents and control them over the /ws API (docs/bridge.md, Web
// panel). Every piece of text from the server goes in with textContent.
'use strict';

const $ = (id) => document.getElementById(id);
const KEY = 'h0x-session';
const PING_MS = 25000; // the server drops a connection silent for 60 s
const DEAD_MS = 50000; // it sends a heartbeat every 20 s
const MAX_LINES = 200;
const FIRST_LINES = 80;

const store = {
  get() { try { return localStorage.getItem(KEY); } catch { return null; } },
  set(v) { try { localStorage.setItem(KEY, v); } catch { /* private mode: this tab only */ } },
  drop() { try { localStorage.removeItem(KEY); } catch { /* nothing stored */ } },
};

let session = store.get();
let pairCode = new URLSearchParams(location.hash.slice(1)).get('pair');
// The one-time code leaves the address bar (and the history) at once.
if (location.hash) history.replaceState(null, '', location.pathname + location.search);

let ws = null;
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

// Reached through a Cloudflare tunnel (REM-3b): say it is not end-to-end.
function tunnel(data) {
  if (data.tunnel) $('tunnel').hidden = false;
}

function needPair(why) {
  session = null;
  store.drop();
  ready = false;
  $('pair-why').textContent = why || '';
  pill('Not paired', '');
  show('pair');
}

// ---- connection ----

function connect() {
  clearTimeout(retryTimer);
  if (ws) return;
  if (!session && !pairCode) return needPair('This device is not paired yet.');
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const sock = new WebSocket(`${proto}//${location.host}/ws`);
  ws = sock;
  let pairing = false;
  let refused = '';
  sock.addEventListener('open', () => {
    lastHeard = Date.now();
    clearInterval(pingTimer);
    pingTimer = setInterval(() => {
      if (Date.now() - lastHeard > DEAD_MS) sock.close();
      else send('ping', null);
    }, PING_MS);
    if (pairCode) {
      pairing = true;
      const label = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || 'browser';
      send('pair', { token: pairCode, label: `${label} browser` });
      pairCode = null; // works once, either way
    } else {
      send('auth', { session });
    }
  });
  sock.addEventListener('message', (event) => {
    lastHeard = Date.now();
    let msg;
    try { msg = JSON.parse(event.data); } catch { return; }
    if (!ready && msg.name === 'error') refused = String((msg.data && msg.data.message) || 'refused');
    receive(msg.name, msg.data || {});
  });
  sock.addEventListener('close', () => {
    clearInterval(pingTimer);
    if (ws === sock) ws = null;
    const was = ready;
    ready = false;
    for (const [, done] of pending) done({ kind: 'failed', message: 'connection lost' });
    pending.clear();
    if (!was && pairing) return needPair(refused ? `Pairing failed: ${refused}` : 'Pairing failed.');
    if (!was && refused && /pair/i.test(refused)) return needPair(`This device is no longer paired (${refused}).`);
    if (pairCode) return connect(); // a code was typed while this one closed
    if (!session) return needPair('');
    const wait = Math.min(30, 2 ** Math.min(retries, 5));
    retries += 1;
    pill(`Reconnecting in ${wait}s`, 'wait');
    retryTimer = setTimeout(connect, wait * 1000);
  });
}

function send(name, data) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ name, data }));
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
$('pair-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const code = $('pair-code').value.trim();
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

const local = ['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);
$('insecure').hidden = location.protocol !== 'http:' || local;
alertsButton();
pill('Connecting', 'wait');
connect();
