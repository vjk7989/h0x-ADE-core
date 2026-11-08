// The panel's own crypto (app.js, ADR-064) against e2e_vectors.json, on both
// of its paths: WebCrypto, and noble.js for plain-http pages without
// crypto.subtle. Both must reproduce every public value, key and frame byte for
// byte and must open each other's frames. The code under test runs inside the
// same vm page as in app.test.mjs; node:crypto only supplies public keys.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createECDH } from 'node:crypto';
import { V, hex, load, unhex } from './fakepage.mjs';

const ecPub = (() => {
  const e = createECDH('prime256v1');
  e.setPrivateKey(Buffer.from(V.ec_priv, 'hex'));
  return new Uint8Array(e.getPublicKey(null, 'uncompressed'));
})();
const enc = (text) => new TextEncoder().encode(text);
// A plain Uint8Array: noble, inside its page, does not take a node Buffer.
const u8 = (...parts) => Uint8Array.from(Buffer.concat(parts));

async function pages() {
  const web = load({ pinned: null });
  const noble = load({ pinned: null, subtle: false });
  return [
    { name: 'webcrypto', w: web, be: await web.run('backend()') },
    { name: 'noble', w: noble, be: await noble.run('backend()') },
  ];
}

// The handshake state the vectors fix: D and Ec from their scalars.
async function fixed({ w, be }) {
  const dev = await be.importPrivate(unhex(V.d_priv), unhex(V.d_pub));
  const eph = await be.importPrivate(unhex(V.ec_priv), ecPub);
  const nonce = unhex(V.c_nonce);
  const text = w.run('clientHello')(dev.pub, eph.pub, nonce);
  return { be, dev, eph, nonce, text, hello: enc(text) };
}

test('each page picks its backend and fetches noble.js only when it must', async () => {
  const [web, noble] = await pages();
  assert.equal(web.be.kind, 'webcrypto');
  assert.deepEqual(web.w.scripts, []);
  assert.equal(noble.be.kind, 'noble');
  assert.deepEqual(noble.w.scripts, ['/noble.js']);
});

test('both backends reproduce the hello, fingerprint, ecdh outputs and keys of the vectors', async () => {
  for (const page of await pages()) {
    const { name, be, w } = page;
    const hs = await fixed(page);
    assert.equal(hs.text, V.client_hello, name);
    assert.equal(w.run('b64')(await be.sha256(unhex(V.s_pub))), V.fp, name);

    const sPub = unhex(V.s_pub);
    const dPub = unhex(V.d_pub);
    const esPub = w.run('unb64')(JSON.parse(V.server_hello).e);
    const es = await be.importPrivate(unhex(V.es_priv), esPub);
    const clientIkm = u8(await be.ecdh(hs.eph.secret, esPub), await be.ecdh(hs.eph.secret, sPub), await be.ecdh(hs.dev.secret, esPub));
    assert.equal(hex(clientIkm), V.ikm, name);
    const sKey = await be.importPrivate(unhex(V.s_priv), sPub);
    const serverIkm = u8(await be.ecdh(es.secret, ecPub), await be.ecdh(sKey.secret, ecPub), await be.ecdh(es.secret, dPub));
    assert.equal(hex(serverIkm), V.ikm, `${name}: the server's mirror`);

    const info = u8(enc('h0x-bridge v1'), await be.sha256(u8(enc(V.client_hello), enc(V.server_hello))));
    const okm = await be.hkdf(unhex(V.ikm), u8(unhex(V.c_nonce), unhex(V.s_nonce)), info);
    assert.equal(hex(okm.slice(0, 32)), V.k_c2s, name);
    assert.equal(hex(okm.slice(32, 64)), V.k_s2c, name);
  }
});

test('both backends seal and open every frame of the vectors', async () => {
  for (const page of await pages()) {
    const { name, be, w } = page;
    const hs = await fixed(page);
    const lanes = await w.run('establish')(hs, V.server_hello, V.fp);
    const frame = w.run('frame');
    for (const f of V.frames.c2s) assert.equal(hex(await frame(be, lanes.out, 'seal', enc(f.plain))), f.wire, `${name} c2s ${f.plain}`);
    for (const f of V.frames.s2c) {
      const plain = await frame(be, lanes.inn, 'open', unhex(f.wire));
      assert.equal(new TextDecoder().decode(plain), f.plain, `${name} s2c`);
    }
    assert.equal(lanes.out.ctr, BigInt(V.frames.c2s.length));
    assert.equal(lanes.inn.ctr, BigInt(V.frames.s2c.length));
  }
});

test('a wrong fingerprint is a block, a reformatted hello gives other keys, bad keys are refused', async () => {
  for (const page of await pages()) {
    const { name, be, w } = page;
    const hs = await fixed(page);
    const establish = w.run('establish');

    const wrong = await establish(hs, V.server_hello, `${V.fp.slice(0, 42)}${V.fp.endsWith('A') ? 'B' : 'A'}`).catch((e) => e);
    assert.equal(wrong.block, true, name);
    for (const want of ['', V.fp.slice(0, 42), `${V.fp}A`]) {
      assert.equal((await establish(hs, V.server_hello, want).catch((e) => e)).block, true, `${name}: "${want}"`);
    }

    // One space in the server hello: same values, other transcript, other keys.
    const spaced = V.server_hello.replace('{"v":1,', '{"v": 1,');
    const lanes = await establish(hs, spaced, V.fp);
    const first = await w.run('frame')(be, lanes.inn, 'open', unhex(V.frames.s2c[0].wire)).catch((e) => e);
    assert.ok(first && first.message, `${name}: the first frame no longer opens`);

    // Off the curve, compressed, wrong length: refused by ecdh on both paths.
    const off = Uint8Array.from([4, ...new Uint8Array(64).fill(1)]);
    await assert.rejects(be.ecdh(hs.eph.secret, off), name);
    await assert.rejects(be.ecdh(hs.eph.secret, Uint8Array.from([2, ...unhex(V.s_pub).slice(1, 33)])), name);
    await assert.rejects(be.ecdh(hs.eph.secret, unhex(V.s_pub).slice(0, 64)), name);
  }
});

test('frames in the wrong order, direction or with a flipped bit fail and do not advance the counter', async () => {
  for (const page of await pages()) {
    const { name, be, w } = page;
    const frame = w.run('frame');
    const lane = (dir) => w.run('lane')(be, unhex(V.k_c2s), dir);
    const [first, second] = V.frames.c2s.map((f) => unhex(f.wire));
    const lane1 = await lane(1);
    await assert.rejects(frame(be, lane1, 'open', second), `${name}: reorder`);
    const bad = first.slice();
    bad[bad.length - 1] ^= 1;
    await assert.rejects(frame(be, lane1, 'open', bad), `${name}: tag`);
    await assert.rejects(frame(be, lane1, 'open', first.slice(0, 15)), `${name}: short`);
    await assert.rejects(frame(be, await lane(2), 'open', first), `${name}: other direction`);
    assert.equal(lane1.ctr, 0n);
    assert.equal(new TextDecoder().decode(await frame(be, lane1, 'open', first)), '');
    await assert.rejects(frame(be, lane1, 'open', first), `${name}: replay`);
  }
});

test('the counter stops before it would wrap', async () => {
  for (const page of await pages()) {
    const { name, be, w } = page;
    const lane = await w.run('lane')(be, unhex(V.k_c2s), 1);
    lane.ctr = (1n << 64n) - 2n;
    await w.run('frame')(be, lane, 'seal', new Uint8Array(1));
    assert.equal(lane.ctr, (1n << 64n) - 1n, name);
    await assert.rejects(w.run('frame')(be, lane, 'seal', new Uint8Array(1)), /counter exhausted/, name);
  }
});

test('the two backends interoperate: same ecdh, same keys, each opens the other\'s frames', async () => {
  const [web, noble] = await pages();
  // Fresh keys on each side, shared secrets compared across backends.
  const a = await web.be.generate();
  const b = await noble.be.generate();
  assert.equal(hex(await web.be.ecdh(a.secret, b.pub)), hex(await noble.be.ecdh(b.secret, a.pub)));

  const raw = unhex(V.k_s2c);
  const texts = ['', 'x', 'ping é', '{"name":"snapshot","data":{"lines":["t1 · claude · C:\\\\Users\\\\me · working"]}}', 'é'.repeat(5000), '\u{1f600}'];
  for (const [from, to] of [[web, noble], [noble, web]]) {
    const sender = await from.w.run('lane')(from.be, raw, 2);
    const receiver = await to.w.run('lane')(to.be, raw, 2);
    for (const t of texts) {
      const wire = await from.w.run('frame')(from.be, sender, 'seal', enc(t));
      assert.equal(wire.length, enc(t).length + 16);
      const plain = await to.w.run('frame')(to.be, receiver, 'open', wire);
      assert.equal(new TextDecoder().decode(plain), t, `${from.name} to ${to.name}`);
    }
  }
});

test('base64url in the page is strict like the Rust side', async () => {
  const [{ w }] = await pages();
  const b64 = w.run('b64');
  const unb64 = w.run('unb64');
  assert.equal(b64(Uint8Array.of(0xfb, 0xff)), '-_8');
  assert.equal(hex(unb64('-_8')), 'fbff');
  assert.equal(unb64('').length, 0);
  for (const bad of ['AA==', 'A', 'a+b/', 'AB', 'A A', 'é']) assert.throws(() => unb64(bad), bad);
  for (const n of [0, 1, 2, 3, 32, 65]) {
    const bytes = Uint8Array.from({ length: n }, (_, i) => (i * 37) & 255);
    assert.equal(b64(bytes), Buffer.from(bytes).toString('base64url'));
    assert.equal(hex(unb64(b64(bytes))), hex(bytes));
  }
});

test('the fingerprint comparison looks at every character and the length', async () => {
  const [{ w }] = await pages();
  const same = w.run('same');
  assert.equal(same(V.fp, V.fp), true);
  assert.equal(same(V.fp, `${V.fp.slice(0, -1)}x`), false);
  assert.equal(same(V.fp, V.fp.slice(1)), false);
  assert.equal(same('', ''), true);
  assert.equal(same(V.fp, ''), false);
});
