// The web panel's logic, run by `node --test` against a fake page and a fake
// encrypting server (fakepage.mjs; no DOM library, no dependency). Run from
// assets.rs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createECDH, createHash } from 'node:crypto';
import { V, arrayBuffer, load } from './fakepage.mjs';

// A page with an authenticated socket.
async function connected(opts = {}) {
  const w = load({ stored: 'S1', ...opts });
  await w.ws().open();
  await w.ws().msg('hello', null);
  return w;
}

test('a pair code in the address is stripped, sent once as pair, and the session is used on reload', async () => {
  const w = load({ href: 'http://192.168.1.20:8377/?v=1#pair=ABCD2345' });
  assert.deepEqual(w.history, [['replace', null, '', '/?v=1']]);
  assert.equal(w.ws().url, 'ws://192.168.1.20:8377/ws');
  await w.ws().open();
  assert.deepEqual(w.ws().sent, [{ name: 'pair', data: { token: 'ABCD2345', label: 'Linux browser' } }]);
  await w.ws().msg('paired', { session: 'S9', device: 'Linux browser' });
  assert.equal(w.saved.get('h0x-session'), 'S9');
  assert.equal(w.$('list').hidden, false);
  await w.ws().close();
  assert.equal(w.reconnect(), 1000);
  await w.ws().open();
  assert.deepEqual(w.ws().sent, [{ name: 'auth', data: { session: 'S9' } }], 'the code is not sent again');

  const reload = load({ stored: 'S9' });
  assert.deepEqual(reload.history, [], 'nothing to strip');
  await reload.ws().open();
  assert.deepEqual(reload.ws().sent, [{ name: 'auth', data: { session: 'S9' } }]);
});

test('the pair fragment parses empty, padded, very long and mixed fragments', async () => {
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
    await w.ws().open();
    assert.deepEqual(w.ws().sent[0].data.token, token, hash);
  }
  for (const hash of ['#pair=', '#pair', '#x=1', '#']) {
    const w = load({ href: `http://192.168.1.20:8377/${hash}` });
    assert.equal(w.sockets.length, 0, `${hash}: no code, no socket`);
    assert.equal(w.$('pair').hidden, false, hash);
    assert.equal(w.$('pair-why').textContent, 'This device is not paired yet.', hash);
  }
});

test('a refused pairing or a forgotten session brings the pair screen back', async () => {
  const w = load({ href: 'http://192.168.1.20:8377/#pair=BAD' });
  await w.ws().open();
  await w.ws().msg('error', { message: 'pairing code expired' });
  await w.ws().close();
  assert.equal(w.$('pair').hidden, false);
  assert.equal(w.$('pair-why').textContent, 'Pairing failed: pairing code expired');
  assert.equal(w.timers.size, 0, 'no retry with a used code');

  const silent = load({ href: 'http://192.168.1.20:8377/#pair=BAD' });
  await silent.ws().open();
  await silent.ws().close();
  assert.equal(silent.$('pair-why').textContent, 'Pairing failed.');

  const forgot = load({ stored: 'S1' });
  await forgot.ws().open();
  await forgot.ws().msg('error', { message: 'unknown or revoked session; pair again' });
  await forgot.ws().close();
  assert.equal(forgot.$('pair').hidden, false);
  assert.equal(forgot.saved.has('h0x-session'), false, 'the dead session is dropped');

  const busy = load({ stored: 'S1' });
  await busy.ws().open();
  await busy.ws().msg('error', { message: 'server busy' });
  await busy.ws().close();
  assert.equal(busy.$('pair').hidden, true, 'other refusals retry');
  assert.equal(busy.reconnect(), 1000);
});

test('a typed code is trimmed and an empty one is ignored', async () => {
  const w = load();
  assert.equal(w.sockets.length, 0);
  w.$('pair-code').value = '   ';
  w.$('pair-form').fire('submit');
  assert.equal(w.sockets.length, 0);
  w.$('pair-code').value = '  abcd2345 ';
  w.$('pair-form').fire('submit');
  await w.ws().open();
  assert.equal(w.ws().sent[0].data.token, 'ABCD2345');
  assert.equal(w.$('pair-code').value, '');
});

test('a typed code is cut like one from the link', async () => {
  for (const typed of ['abcd2345#x', 'ABCD2345&pair=1', 'abcd2345 extra', '\tabcd2345\n']) {
    const w = load();
    w.$('pair-code').value = typed;
    w.$('pair-form').fire('submit');
    await w.ws().open();
    assert.equal(w.ws().sent[0].data.token, 'ABCD2345', JSON.stringify(typed));
  }
  const none = load();
  none.$('pair-code').value = '#x';
  none.$('pair-form').fire('submit');
  assert.equal(none.sockets.length, 0, 'nothing left of the code');
});

test('a localStorage that throws still pairs and reconnects within the tab', async () => {
  const w = load({ href: `http://192.168.1.20:8377/#pair=ABCD2345&fp=${V.fp}`, storage: 'throws', pinned: null });
  await w.ws().open();
  await w.ws().msg('paired', { session: 'S9' });
  assert.equal(w.$('list').hidden, false);
  await w.ws().close();
  assert.equal(w.reconnect(), 1000);
  await w.ws().open();
  assert.deepEqual(w.ws().sent, [{ name: 'auth', data: { session: 'S9' } }]);
  await w.ws().msg('error', { message: 'unknown or revoked session; pair again' });
  await w.ws().close();
  assert.equal(w.$('pair').hidden, false, 'dropping the session does not throw either');
});

test('reconnects back off 1, 2, 4, 8, 16, 30, 30 s and reset once heard', async () => {
  const w = load({ stored: 'S1' });
  const waits = [];
  for (let i = 0; i < 7; i += 1) {
    await w.ws().close();
    waits.push(w.reconnect());
  }
  assert.deepEqual(waits, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
  await w.ws().open();
  await w.ws().msg('hello', null);
  await w.ws().close();
  assert.equal(w.$('pill').textContent, 'Reconnecting in 1s');
  assert.equal(w.reconnect(), 1000);
});

test('pings every 25 s and gives the socket up after 50 s of silence', async () => {
  const w = await connected();
  const pings = [...w.timers.values()].filter((t) => t.repeat);
  assert.deepEqual(pings.map((t) => t.ms), [25000]);
  const tick = pings[0].fn;
  w.now = 25000;
  tick();
  await w.settle();
  assert.deepEqual(w.ws().sent.at(-1), { name: 'ping', data: null });
  await w.ws().msg('pong', null); // heard at 25 s
  w.now = 75000; // exactly 50 s: still alive
  tick();
  await w.settle();
  assert.equal(w.ws().readyState, 1);
  w.now = 75001;
  tick();
  await w.settle();
  assert.equal(w.ws().readyState, 3);
  assert.equal([...w.timers.values()].some((t) => t.repeat), false, 'the ping stops with the socket');
});

test('the agents list puts needs-you first, then error, working, done, idle, ended', async () => {
  const w = await connected();
  await w.ws().msg('snapshot', {
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
    await w.ws().msg('snapshot', { lines });
    assert.equal(w.$('agents').children.length, 0);
    assert.equal(w.$('empty').hidden, false);
  }
});

test('the agent view reads 80 lines, then 60 more up to 200', async () => {
  const w = await connected();
  await w.ws().msg('snapshot', { lines: ['t2 · claude · ~/a · blocked'] });
  w.$('agents').children[0].children[0].fire('click');
  await w.settle();
  assert.equal(w.$('agent').hidden, false);
  const reads = () => w.commands().filter((c) => c.cmd === 'read').map((c) => c.lines);
  assert.deepEqual(reads(), [80]);
  assert.equal(w.$('more').hidden, false);
  w.$('more').fire('click');
  assert.equal(w.$('more').hidden, false);
  w.$('more').fire('click');
  assert.equal(w.$('more').hidden, true);
  w.$('more').fire('click');
  await w.settle();
  assert.deepEqual(reads(), [80, 140, 200, 200]);
  const last = w.commands().at(-1);
  await w.ws().msg('reply', { id: last.id, kind: 'done', text: '' });
  assert.equal(w.$('out').textContent, '(no output yet)');
});

test('quick keys and actions go to the open agent; nothing goes before the socket is ready', async () => {
  const early = load({ stored: 'S1' });
  await early.ws().open();
  early.keys[0].fire('click');
  await early.settle();
  assert.deepEqual(early.commands(), []);
  assert.deepEqual(early.toasts(), ['Not connected yet.']);

  const w = await connected();
  w.run("openAgent('t2')");
  for (const b of w.keys) b.fire('click');
  for (const b of w.cmds) b.fire('click');
  await w.settle();
  const sent = w.commands().filter((c) => c.cmd !== 'read').map(({ id, ...c }) => c);
  assert.deepEqual(sent, [
    ...['enter', 'esc', 'ctrl+c', 'up', 'down', 'y', 'n'].map((k) => ({ cmd: 'keys', terminal: 't2', keys: [k] })),
    ...['approve', 'deny', 'interrupt', 'kill'].map((cmd) => ({ cmd, terminal: 't2' })),
  ]);
});

test('a command needing confirmation asks first; cancel sends nothing', async () => {
  const w = await connected();
  w.run("openAgent('t2')");
  w.cmds.find((b) => b.dataset.cmd === 'kill').fire('click');
  await w.settle();
  const kill = w.commands().at(-1);
  await w.ws().msg('reply', { id: kill.id, kind: 'needs_confirm', what: 'Kill t2?', token: 'tok' });
  assert.equal(w.$('confirm').open, true);
  assert.equal(w.$('confirm-what').textContent, 'Kill t2?');
  w.$('confirm-no').fire('click');
  await w.settle();
  assert.equal(w.$('confirm').open, false);
  assert.equal(w.commands().at(-1).id, kill.id, 'nothing sent on cancel');

  await w.ws().msg('reply', { id: kill.id, kind: 'needs_confirm', what: 'Kill t2?', token: 'tok' });
  w.$('confirm-yes').fire('click');
  await w.settle();
  assert.equal(w.commands().at(-1).cmd, 'confirm');
  assert.equal(w.commands().at(-1).token, 'tok');
  w.$('confirm-yes').fire('click'); // a second press has nothing to confirm
  await w.settle();
  assert.equal(w.commands().filter((c) => c.cmd === 'confirm').length, 1);
});

test('run needs a folder and a command, trims them and keeps Windows paths', async () => {
  const w = await connected();
  w.$('run-open').fire('click');
  assert.equal(w.$('run').open, true);
  for (const [folder, command] of [['', 'claude'], ['~/p', '   '], ['  ', '  ']]) {
    w.$('run-folder').value = folder;
    w.$('run-command').value = command;
    w.$('run-form').fire('submit');
  }
  await w.settle();
  assert.deepEqual(w.commands(), []);
  assert.equal(w.$('run').open, true);
  w.$('run-folder').value = '  C:\\Users\\me\\my proj  ';
  w.$('run-command').value = ' claude ';
  w.$('run-form').fire('submit');
  await w.settle();
  assert.equal(w.$('run').open, false);
  const { id, ...run } = w.commands()[0];
  assert.deepEqual(run, { cmd: 'run', folder: 'C:\\Users\\me\\my proj', command: 'claude' });
});

test('lock asks, then locks the panel', async () => {
  const w = await connected();
  assert.equal(w.$('lock').hidden, false);
  w.$('lock').fire('click');
  assert.equal(w.$('confirm').open, true);
  w.$('confirm-yes').fire('click');
  await w.settle();
  const lock = w.commands().at(-1);
  assert.equal(lock.cmd, 'lock');
  await w.ws().msg('reply', { id: lock.id, kind: 'done', text: 'locked' });
  assert.equal(w.$('locked').hidden, false);
  assert.equal(w.$('lock').disabled, true);
  assert.equal(w.$('pill').textContent, 'Locked');
});

test('notices toast; browser notifications wait for the button and permission', async () => {
  const w = await connected({ secure: true });
  assert.equal(w.$('alerts').hidden, false, 'offered on a secure page');
  await w.ws().msg('notice', { terminal: 't2', agent: 'claude', kind: 'needs_you', excerpt: 'Allow?' });
  assert.deepEqual(w.toasts(), ['claude needs you: Allow?']);
  assert.equal(w.$('toasts').children[0].className, 'toast needs');
  assert.deepEqual(w.notes, [], 'no notification before permission');
  assert.equal(w.asked, 0);
  w.$('alerts').fire('click');
  assert.equal(w.asked, 1);
  for (let i = 0; i < 6; i += 1) await w.ws().msg('notice', { terminal: `t${i}`, kind: 'done' });
  assert.equal(w.$('toasts').children.length, 4, 'at most four toasts');
  assert.equal(w.toasts().at(-1), 't5 is done');

  assert.equal((await connected({ secure: false })).$('alerts').hidden, true, 'not offered over plain http');
  assert.equal((await connected({ secure: true, permission: 'denied' })).$('alerts').hidden, true);

  const away = await connected({ permission: 'granted', hidden: true });
  await away.ws().msg('notice', { terminal: 't2', agent: 'claude', kind: 'error', excerpt: null });
  assert.deepEqual(away.notes, [{ title: 'HyperOx: claude hit an error', opts: { body: '', tag: 't2' } }]);
  const looking = await connected({ permission: 'granted', hidden: false });
  await looking.ws().msg('notice', { terminal: 't2', kind: 'done' });
  assert.deepEqual(looking.notes, [], 'no notification while the page is in view');
});

test('the plain-http banner shows off localhost only, the tunnel banner on request', async () => {
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
  assert.equal((await connected()).$('tunnel').hidden, true);
  const tunnel = load({ stored: 'S1' });
  await tunnel.ws().open();
  await tunnel.ws().msg('hello', { tunnel: true });
  assert.equal(tunnel.$('tunnel').hidden, false);
});

// ---- end-to-end encryption (ADR-064) ----

const LINK = `http://192.168.1.20:8377/#pair=ABCD2345&fp=${V.fp}`;
const NOBLE_ONLY = { subtle: false };

// The fingerprint a server with this identity scalar has.
function fpOf(priv) {
  const key = createECDH('prime256v1');
  key.setPrivateKey(Buffer.from(priv, 'hex'));
  return createHash('sha256').update(key.getPublicKey(null, 'uncompressed')).digest('base64url');
}

test('the first frame is the plain client hello, every later frame is sealed binary', async () => {
  for (const opts of [{}, NOBLE_ONLY]) {
    const w = load({ href: LINK, pinned: null, ...opts });
    const sock = w.ws();
    assert.equal(sock.binaryType, 'arraybuffer');
    await sock.open();
    assert.equal(sock.hellos.length, 1);
    const hello = JSON.parse(sock.hellos[0]);
    assert.deepEqual(Object.keys(hello), ['v', 'd', 'e', 'n']);
    assert.equal(hello.v, 1);
    assert.equal(sock.hellos[0], `{"v":1,"d":"${hello.d}","e":"${hello.e}","n":"${hello.n}"}`, 'exact field order, no spaces');
    assert.equal(Buffer.from(hello.d, 'base64url').length, 65);
    assert.equal(Buffer.from(hello.e, 'base64url').length, 65);
    assert.equal(Buffer.from(hello.n, 'base64url').length, 32);
    assert.deepEqual(sock.sent, [{ name: 'pair', data: { token: 'ABCD2345', label: 'Linux browser' } }]);
    await sock.msg('paired', { session: 'S9' });
    w.keys[0].fire('click');
    await w.settle();
    assert.deepEqual(sock.leaks, [], 'nothing but the hello went out as text');
    assert.equal(w.scripts.length, opts.subtle === false ? 1 : 0, 'noble.js is fetched only without crypto.subtle');
  }
});

test('the fingerprint is pinned only once the pairing succeeded, then used without the link', async () => {
  const w = load({ href: LINK, pinned: null });
  await w.ws().open();
  assert.equal(w.saved.has('h0x-fp'), false, 'not pinned before the server accepted the pairing');
  await w.ws().msg('paired', { session: 'S9' });
  assert.equal(w.saved.get('h0x-fp'), V.fp);

  const reload = load({ stored: 'S9', pinned: V.fp });
  await reload.ws().open();
  assert.deepEqual(reload.ws().sent, [{ name: 'auth', data: { session: 'S9' } }]);

  const refused = load({ href: LINK, pinned: null });
  await refused.ws().open();
  await refused.ws().msg('error', { message: 'pairing code expired' });
  await refused.ws().close();
  assert.equal(refused.saved.has('h0x-fp'), false, 'a refused pairing pins nothing');
});

test('the device key is made once per origin and reused', async () => {
  for (const opts of [{}, NOBLE_ONLY]) {
    const idb = new Map();
    const first = load({ stored: 'S1', idb, ...opts });
    await first.ws().open();
    const second = load({ stored: 'S1', idb, ...opts });
    await second.ws().open();
    const d = (w) => JSON.parse(w.ws().hellos[0]).d;
    assert.equal(d(first), d(second));
    assert.equal(idb.size, 1);
    const other = load({ stored: 'S1', ...opts });
    await other.ws().open();
    assert.notEqual(d(other), d(first), 'another origin has its own key');
  }
  // WebCrypto keeps a key that cannot be exported, noble only the scalar.
  const idb = new Map();
  const w = load({ stored: 'S1', idb });
  await w.ws().open();
  const rec = idb.get('device');
  assert.equal(rec.kind, 'webcrypto');
  assert.equal(rec.secret.extractable, false);
  assert.equal(rec.pub.length, 65);
});

test('a visit with no fingerprint anywhere says to scan the QR code again and does not connect', async () => {
  for (const href of ['http://192.168.1.20:8377/#pair=ABCD2345', 'http://192.168.1.20:8377/#pair=ABCD2345&fp=']) {
    const w = load({ href, pinned: null });
    assert.equal(w.sockets.length, 0, href);
    assert.equal(w.$('pair').hidden, false);
    assert.match(w.$('pair-why').textContent, /^Scan the QR code again/);
  }
  const typed = load({ pinned: null });
  typed.$('pair-code').value = 'abcd2345';
  typed.$('pair-form').fire('submit');
  assert.equal(typed.sockets.length, 0, 'a typed code cannot pair without a fingerprint');
  assert.match(typed.$('pair-why').textContent, /^Scan the QR code again/);

  const old = load({ stored: 'S1', pinned: null });
  assert.equal(old.sockets.length, 0, 'a session from before encryption has no fingerprint to check');
  assert.match(old.$('pair-why').textContent, /^Scan the QR code again/);
  assert.equal(old.saved.has('h0x-session'), false);
});

test('a link whose fingerprint differs from the pinned one is refused until the computer is forgotten', async () => {
  const serverKey = '7'.repeat(64);
  const w = load({ href: `http://192.168.1.20:8377/#pair=ABCD2345&fp=${fpOf(serverKey)}`, stored: 'S1', pinned: V.fp, serverKey });
  assert.equal(w.sockets.length, 0);
  assert.equal(w.$('pair').hidden, false);
  assert.equal(w.$('forget').hidden, false);
  assert.match(w.$('pair-why').textContent, /different computer/);
  assert.equal(w.saved.get('h0x-fp'), V.fp, 'the pin is untouched');
  assert.equal(w.saved.get('h0x-session'), 'S1');
  w.run('wake()'); // no-op for a blocked page
  w.$('pair-code').value = 'abcd2345';
  w.$('pair-form').fire('submit');
  assert.equal(w.sockets.length, 0, 'a typed code does not get around it');

  w.$('forget').fire('click');
  await w.settle();
  assert.equal(w.saved.has('h0x-fp'), false);
  assert.equal(w.saved.has('h0x-session'), false);
  assert.equal(w.sockets.length, 1, 'the link now pairs with the new computer');
  await w.ws().open();
  assert.equal(w.ws().sent[0].name, 'pair');
});

test('forget without a link returns to the pair screen', async () => {
  const w = await connected();
  w.run('needPair("")');
  assert.equal(w.$('forget').hidden, false, 'offered while a computer is pinned');
  w.$('forget').fire('click');
  await w.settle();
  assert.equal(w.saved.has('h0x-fp'), false);
  assert.equal(w.$('forget').hidden, true, 'nothing to forget any more');
  assert.equal(w.$('pair').hidden, false);
  assert.equal(w.$('pair-why').textContent, '');
});

test('a server with another identity is refused before anything is sent', async () => {
  for (const opts of [{}, NOBLE_ONLY]) {
    const w = load({ stored: 'S1', serverKey: '7'.repeat(64), ...opts });
    await w.ws().open();
    assert.equal(w.ws().readyState, 3, 'closed');
    assert.deepEqual(w.ws().sent, [], 'no session token went out');
    assert.deepEqual(w.ws().leaks, []);
    assert.equal(w.$('pair').hidden, false);
    assert.equal(w.$('forget').hidden, false);
    assert.match(w.$('pair-why').textContent, /not the computer this device was paired with/);
    assert.equal(w.timers.size, 0, 'no reconnect');
    assert.equal(w.saved.get('h0x-fp'), V.fp);
    w.run('wake()');
    w.run('connect()');
    assert.equal(w.sockets.length, 1, 'a blocked page stays blocked');
  }
});

test('a plain text frame after the handshake closes the socket', async () => {
  const w = await connected();
  const sock = w.ws();
  sock.emit('message', { data: JSON.stringify({ name: 'notice', data: { terminal: 't2', kind: 'done' } }) });
  await w.settle();
  assert.equal(sock.readyState, 3);
  assert.deepEqual(w.toasts(), ['Secure connection failed: unencrypted frame']);
  assert.equal(w.$('toasts').children[0].className, 'toast bad');
  assert.equal(w.timers.size, 1, 'it reconnects with a fresh handshake, never in the clear');
  assert.equal(w.reconnect(), 1000);
  await w.ws().open();
  assert.equal(w.ws().hellos.length, 1);
  assert.deepEqual(w.ws().leaks, []);
});

test('a tampered, replayed or reordered binary frame closes the socket', async () => {
  const seal = (w, name) => w.ws().srv.seal(Buffer.from(JSON.stringify({ name, data: {} })));
  const attacks = {
    'flipped bit': (w) => { const f = seal(w, 'notice'); f[2] ^= 1; return [f]; },
    'flipped tag bit': (w) => { const f = seal(w, 'notice'); f[f.length - 1] ^= 1; return [f]; },
    'too short for a tag': () => [Buffer.alloc(15)],
    empty: () => [Buffer.alloc(0)],
    'replay': (w) => { const f = seal(w, 'notice'); return [f, f]; },
    'reorder': (w) => { const a = seal(w, 'notice'); const b = seal(w, 'notice'); return [b, a]; },
  };
  for (const opts of [{}, NOBLE_ONLY]) {
    for (const [name, make] of Object.entries(attacks)) {
      const w = await connected(opts);
      const sock = w.ws();
      const frames = make(w);
      for (const f of frames) sock.emit('message', { data: arrayBuffer(f) });
      await w.settle();
      assert.equal(sock.readyState, 3, name);
      assert.match(w.toasts().at(-1), /^Secure connection failed/, name);
      assert.deepEqual(sock.leaks, [], name);
    }
  }
});

test('a good frame after a rejected one is not accepted: the socket is already closed', async () => {
  const w = await connected();
  const sock = w.ws();
  const bad = sock.srv.seal(Buffer.from('{"name":"notice","data":{"terminal":"t1","kind":"done"}}'));
  const good = sock.srv.seal(Buffer.from('{"name":"notice","data":{"terminal":"t2","kind":"done"}}'));
  bad[0] ^= 1;
  sock.emit('message', { data: arrayBuffer(bad) });
  sock.emit('message', { data: arrayBuffer(good) });
  await w.settle();
  assert.equal(sock.readyState, 3);
  assert.deepEqual(w.toasts().filter((t) => t.endsWith('is done')), []);
});

test('a binary frame that is not JSON is dropped like before; one that is not UTF-8 closes', async () => {
  const w = await connected();
  const sock = w.ws();
  sock.emit('message', { data: arrayBuffer(sock.srv.seal(Buffer.from('not json'))) });
  await w.settle();
  assert.equal(sock.readyState, 1);
  sock.emit('message', { data: arrayBuffer(sock.srv.seal(Buffer.from([0xff, 0xfe]))) });
  await w.settle();
  assert.equal(sock.readyState, 3);
});

test('during the handshake only the server hello text is accepted', async () => {
  const bad = {
    'binary first': () => arrayBuffer(Buffer.alloc(40)),
    'not json': () => 'hello',
    'wrong version': (hello) => hello.replace('"v":1', '"v":2'),
    'missing nonce': (hello) => hello.replace(/,"n":"[^"]*"/, ''),
    'short nonce': (hello) => hello.replace(/"n":"[^"]*"/, '"n":"AAAA"'),
    'padded base64': (hello) => hello.replace(/"n":"([^"]*)"/, '"n":"$1="'),
  };
  for (const opts of [{}, NOBLE_ONLY]) {
    for (const [name, change] of Object.entries(bad)) {
      const w = load({ stored: 'S1', ...opts });
      const sock = w.ws();
      // The fake server's hello is replaced on its way to the page.
      const emit = sock.emit.bind(sock);
      sock.emit = (type, event) => (type === 'message' && typeof event.data === 'string' && event.data.includes('"s":') ? emit(type, { data: change(event.data) }) : emit(type, event));
      await sock.open();
      assert.equal(sock.readyState, 3, name);
      assert.deepEqual(sock.leaks, [], name);
      assert.match(w.toasts().at(-1) ?? '', /^Secure connection failed/, name);
      assert.equal(w.$('forget').hidden, true, `${name}: a malformed hello is not an impostor`);
    }
  }
});

test('an off-curve server key is refused after the fingerprint check', async () => {
  for (const opts of [{}, NOBLE_ONLY]) {
    const w = load({ stored: 'S1', ...opts });
    const sock = w.ws();
    const emit = sock.emit.bind(sock);
    sock.emit = (type, event) => {
      if (type !== 'message' || typeof event.data !== 'string') return emit(type, event);
      // Same S, so the fingerprint matches; the ephemeral key is not on the curve.
      const hello = JSON.parse(event.data);
      hello.e = Buffer.concat([Buffer.from([4]), Buffer.alloc(64, 1)]).toString('base64url');
      return emit(type, { data: JSON.stringify(hello) });
    };
    await sock.open();
    assert.equal(sock.readyState, 3);
    assert.deepEqual(sock.sent, []);
    assert.match(w.toasts().at(-1), /^Secure connection failed/);
  }
});

test('the page has no way to send before the handshake and nothing after a close', async () => {
  const w = load({ stored: 'S1' });
  const sock = w.ws();
  w.run("send('ping', null)");
  await w.settle();
  assert.equal(sock.hellos.length + sock.sent.length, 0, 'not even a hello before open');
  await sock.open();
  await sock.close();
  w.run("send('ping', null)");
  await w.settle();
  assert.deepEqual(sock.leaks, []);
  assert.equal(sock.sent.filter((m) => m.name === 'ping').length, 0);
});

test('without crypto.subtle the page fetches noble.js once and works the same', async () => {
  const w = load({ href: LINK, pinned: null, subtle: false });
  await w.ws().open();
  await w.ws().msg('paired', { session: 'S9', device: 'Linux browser' });
  assert.deepEqual(w.scripts, ['/noble.js']);
  assert.equal(w.$('list').hidden, false);
  await w.ws().msg('snapshot', { lines: ['t2 · claude · ~/a · blocked'] });
  assert.equal(w.$('agents').children.length, 1);
  w.$('agents').children[0].children[0].fire('click');
  await w.settle();
  assert.equal(w.commands().at(-1).cmd, 'read');
  await w.ws().close();
  w.reconnect();
  await w.ws().open();
  assert.deepEqual(w.scripts, ['/noble.js'], 'not fetched again');
  assert.deepEqual(w.ws().sent, [{ name: 'auth', data: { session: 'S9' } }]);
});

test('the link fingerprint is read like the pair code: decoded, trimmed, first one wins, otherwise exact', async () => {
  const pct = `%${V.fp.charCodeAt(0).toString(16)}${V.fp.slice(1)}`;
  const base = 'http://192.168.1.20:8377/';
  for (const hash of [
    `#pair=ABCD2345&fp=%20${V.fp}%20`,
    `#pair=ABCD2345&fp=${pct}`,
    `#fp=${V.fp}&x=1&pair=ABCD2345`,
    `#pair=ABCD2345&fp=${V.fp}&fp=${'A'.repeat(43)}`,
  ]) {
    const w = load({ href: base + hash, pinned: null });
    await w.ws().open();
    assert.equal(w.ws().sent[0]?.name, 'pair', hash);
    await w.ws().msg('paired', { session: 'S9' });
    assert.equal(w.saved.get('h0x-fp'), V.fp, `${hash}: the decoded fingerprint is pinned`);
  }
  for (const fp of [
    V.fp.toUpperCase(),
    V.fp.toLowerCase(),
    `${V.fp}=`,
    `${V.fp}A`,
    V.fp.slice(1),
    `${V.fp}#junk`,
    '%3Cscript%3E',
    'A'.repeat(100000),
  ]) {
    const w = load({ href: `${base}#pair=ABCD2345&fp=${fp}`, pinned: null });
    const sock = w.ws();
    await sock.open();
    const label = fp.slice(0, 50);
    assert.equal(sock.readyState, 3, label);
    assert.deepEqual(sock.sent, [], `${label}: the pair code never went out`);
    assert.deepEqual(sock.leaks, [], label);
    assert.equal(w.saved.has('h0x-fp'), false, `${label}: nothing pinned`);
    assert.equal(w.timers.size, 0, `${label}: no retry`);
    assert.equal(w.$('forget').hidden, false, label);
    assert.doesNotMatch(w.$('pair-why').textContent, /script/, label);
  }
});

test('an empty, corrupted or unreadable pinned fingerprint never connects unchecked', async () => {
  const empty = load({ stored: 'S1', pinned: '' });
  assert.equal(empty.sockets.length, 0, 'an empty pin is no pin');
  assert.match(empty.$('pair-why').textContent, /^Scan the QR code again/);

  const linked = load({ href: `http://192.168.1.20:8377/#pair=ABCD2345&fp=${V.fp}`, pinned: '' });
  await linked.ws().open();
  assert.equal(linked.ws().sent[0].name, 'pair', 'an empty pin does not block a link');

  const garbage = load({ stored: 'S1', pinned: 'not a fingerprint' });
  await garbage.ws().open();
  assert.equal(garbage.ws().readyState, 3);
  assert.deepEqual(garbage.ws().sent, [], 'the session token is not sent to an unchecked computer');
  assert.match(garbage.$('pair-why').textContent, /not the computer this device was paired with/);

  const relinked = load({ href: `http://192.168.1.20:8377/#pair=ABCD2345&fp=${V.fp}`, pinned: `${V.fp} ` });
  assert.equal(relinked.sockets.length, 0, 'a pin with stray whitespace is still a different computer');
  assert.match(relinked.$('pair-why').textContent, /different computer/);

  const privateMode = load({ href: 'http://192.168.1.20:8377/#pair=ABCD2345', storage: 'throws', stored: 'S1' });
  assert.equal(privateMode.sockets.length, 0, 'a throwing localStorage gives no pin to fall back on');
  assert.match(privateMode.$('pair-why').textContent, /^Scan the QR code again/);
});

test('the server hello arriving twice closes the socket before anything is sent', async () => {
  for (const opts of [{}, NOBLE_ONLY]) {
    const w = load({ stored: 'S1', ...opts });
    const sock = w.ws();
    const emit = sock.emit.bind(sock);
    sock.emit = (type, event) => {
      emit(type, event);
      if (type === 'message' && typeof event.data === 'string') emit(type, event);
    };
    await sock.open();
    assert.equal(sock.readyState, 3);
    assert.deepEqual(sock.sent, [], 'the auth queued after the first hello is dropped');
    assert.deepEqual(sock.leaks, []);
    assert.deepEqual(w.toasts(), ['Secure connection failed: unencrypted frame']);
  }
});

test('a socket closed mid-handshake reconnects with a fresh handshake and ignores the old socket', async () => {
  const w = load({ stored: 'S1' });
  const first = w.ws();
  const emit = first.emit.bind(first);
  let late = null;
  first.emit = (type, event) => (type === 'message' && typeof event.data === 'string' ? (late = event) : emit(type, event));
  await first.open();
  assert.equal(first.hellos.length, 1);
  assert.deepEqual(first.sent, []);
  await first.close();
  assert.deepEqual(w.toasts(), [], 'a dropped connection is not a security failure');
  assert.equal(w.reconnect(), 1000);

  const second = w.ws();
  await second.open();
  const [a, b] = [first, second].map((s) => JSON.parse(s.hellos[0]));
  assert.equal(b.d, a.d, 'the same device key');
  assert.notEqual(b.e, a.e, 'a new ephemeral key');
  assert.notEqual(b.n, a.n, 'a new nonce');
  assert.deepEqual(second.sent, [{ name: 'auth', data: { session: 'S1' } }], 'the new lane starts at counter 0');

  emit('message', late); // the old socket's hello, too late
  await w.settle();
  assert.equal(second.readyState, 1);
  await second.msg('hello', null);
  w.run("send('ping', null)");
  await w.settle();
  assert.equal(second.sent.at(-1).name, 'ping');

  await second.close();
  w.reconnect();
  await w.ws().open();
  assert.deepEqual(w.ws().sent, [{ name: 'auth', data: { session: 'S1' } }], 'counters reset after a used socket too');
});

test('an IndexedDB that cannot open or store the device key fails the connection, never in the clear', async () => {
  const later = (fn) => Promise.resolve().then(fn);
  const cannotOpen = { open() { const req = {}; later(() => req.onerror()); return req; } };
  const cannotStore = {
    open() {
      const req = {};
      later(() => {
        req.result = {
          createObjectStore() {},
          close() {},
          transaction() {
            const tx = {};
            tx.objectStore = () => ({
              get: () => { later(() => tx.oncomplete()); return { result: undefined }; },
              put: () => { later(() => tx.onabort()); return {}; },
            });
            return tx;
          },
        };
        req.onsuccess();
      });
      return req;
    },
  };
  for (const [name, idb, why] of [
    ['missing', undefined, 'this browser cannot keep a device key'],
    ['open error', cannotOpen, 'this browser cannot keep a device key'],
    ['put aborted', cannotStore, 'could not store the device key'],
  ]) {
    const w = load({ stored: 'S1' });
    w.run('globalThis').indexedDB = idb;
    const sock = w.ws();
    await sock.open();
    assert.equal(sock.readyState, 3, name);
    assert.deepEqual([sock.hellos, sock.sent, sock.leaks], [[], [], []], `${name}: nothing went out`);
    assert.deepEqual(w.toasts(), [`Secure connection failed: ${why}`], name);
    assert.equal(w.$('forget').hidden, true, `${name}: not an impostor`);
    assert.equal(w.reconnect(), 1000, `${name}: retried later`);
  }
});

test('two tabs making the device key at once both connect and then share the one kept key', async () => {
  for (const opts of [{}, NOBLE_ONLY]) {
    const idb = new Map();
    const tabs = [load({ stored: 'S1', idb, ...opts }), load({ stored: 'S1', idb, ...opts })];
    await Promise.all(tabs.map((w) => w.ws().open()));
    assert.equal(idb.size, 1);
    const kept = Buffer.from(Array.from(idb.get('device').pub)).toString('base64url');
    const d = (w) => JSON.parse(w.ws().hellos[0]).d;
    assert.ok(tabs.some((w) => d(w) === kept), 'the kept key is one of the two');
    for (const w of tabs) assert.deepEqual(w.ws().sent, [{ name: 'auth', data: { session: 'S1' } }]);
    for (const w of tabs) {
      await w.ws().close();
      w.reconnect();
      await w.ws().open();
      assert.equal(d(w), kept, 'the next handshake uses the kept key');
    }
  }
});

test('noble.js that fails to load or loads empty is a clear error, no socket traffic, retried later', async () => {
  for (const [name, append, why] of [
    ['404', (w) => (el) => { w.scripts.push(el.src); el.fire('error'); }, 'could not load the crypto script'],
    ['empty', (w) => (el) => { w.scripts.push(el.src); el.fire('load'); }, 'crypto script is empty'],
  ]) {
    const w = load({ stored: 'S1', subtle: false });
    const head = w.run('document.head');
    const good = head.append;
    head.append = append(w);
    const sock = w.ws();
    await sock.open();
    assert.equal(sock.readyState, 3, name);
    assert.deepEqual([sock.hellos, sock.sent, sock.leaks], [[], [], []], `${name}: nothing went out`);
    assert.deepEqual(w.toasts(), [`Secure connection failed: ${why}`], name);
    head.append = good;
    assert.equal(w.reconnect(), 1000, name);
    await w.ws().open();
    assert.deepEqual(w.scripts, ['/noble.js', '/noble.js'], `${name}: fetched again after a failure`);
    assert.deepEqual(w.ws().sent, [{ name: 'auth', data: { session: 'S1' } }], name);
  }
});

test('a 1 MiB message goes both ways sealed and the lanes stay in step', async () => {
  for (const opts of [{}, NOBLE_ONLY]) {
    const w = await connected(opts);
    const sock = w.ws();
    const big = 'x'.repeat(1 << 20);
    w.run(`send('echo', { text: 'x'.repeat(${1 << 20}) })`);
    await w.settle();
    assert.equal(sock.sent.at(-1).data.text, big);
    await sock.msg('error', { message: big });
    assert.equal(w.toasts().at(-1), big);
    w.run("send('ping', null)");
    await w.settle();
    assert.equal(sock.sent.at(-1).name, 'ping');
    assert.equal(sock.readyState, 1);
    assert.deepEqual(sock.leaks, []);
  }
});

test('a handshake failure while pairing from a link waits before it reconnects', async () => {
  const w = load({ href: LINK, pinned: null, subtle: false });
  w.run('document.head').append = (el) => { w.scripts.push(el.src); el.fire('error'); };
  const first = w.ws();
  await first.open();
  assert.equal(first.readyState, 3);
  assert.deepEqual(first.sent, []);
  assert.equal(w.sockets.length, 1, 'no new socket at once: a lasting failure would reconnect in a loop with no delay');
});
