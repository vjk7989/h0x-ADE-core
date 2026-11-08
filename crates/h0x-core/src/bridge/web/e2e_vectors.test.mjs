// An independent check of e2e_vectors.json (ADR-064) with WebCrypto only:
// from the file's private scalars and nonces it recomputes every public value
// and compares. The helpers above the tests use nothing but `crypto.subtle`,
// TextEncoder and plain bytes, so the browser client (SEC-3) can reuse them.
// node:crypto appears only in the tests, to derive public keys from scalars
// (WebCrypto cannot) and to cross-check ECDH. Run from assets.rs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createECDH } from 'node:crypto';

const subtle = globalThis.crypto.subtle;
const LABEL = new TextEncoder().encode('h0x-bridge v1');
export const C2S = 1;
export const S2C = 2;

// ---- helpers (browser-safe) ----

const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';

/** base64url without padding. */
export function b64(bytes) {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const chunk = bytes.subarray(i, i + 3);
    const n = (chunk[0] << 16) | ((chunk[1] ?? 0) << 8) | (chunk[2] ?? 0);
    for (let j = 0; j <= chunk.length; j++) out += B64[(n >> (18 - 6 * j)) & 63];
  }
  return out;
}

/** Strict inverse of b64: URL alphabet, no padding, zero trailing bits. */
export function unb64(text) {
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

export function concat(...parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

export async function sha256(bytes) {
  return new Uint8Array(await subtle.digest('SHA-256', bytes));
}

/** `b64(SHA-256(S_pub))`, the pinned server fingerprint. */
export async function fp(serverPublic) {
  return b64(await sha256(serverPublic));
}

/** An ECDH private key from a 32-byte scalar and its 65-byte public key. */
export function importPrivate(scalar, pub) {
  const jwk = { kty: 'EC', crv: 'P-256', d: b64(scalar), x: b64(pub.subarray(1, 33)), y: b64(pub.subarray(33, 65)) };
  return subtle.importKey('jwk', jwk, { name: 'ECDH', namedCurve: 'P-256' }, false, ['deriveBits']);
}

/** A 65-byte SEC1 uncompressed public key; WebCrypto rejects off-curve points. */
export function importPublic(pub) {
  if (pub.length !== 65 || pub[0] !== 4) throw new Error('bad public key');
  return subtle.importKey('raw', pub, { name: 'ECDH', namedCurve: 'P-256' }, false, []);
}

/** The 32-byte x coordinate of the shared point. */
export async function ecdh(privateKey, publicKey) {
  return new Uint8Array(await subtle.deriveBits({ name: 'ECDH', public: publicKey }, privateKey, 256));
}

/** The client hello bytes, field order fixed: v, d, e, n. */
export function clientHello(dPub, ecPub, cNonce) {
  return new TextEncoder().encode(`{"v":1,"d":"${b64(dPub)}","e":"${b64(ecPub)}","n":"${b64(cNonce)}"}`);
}

/** The server hello bytes, field order fixed: v, s, e, n. */
export function serverHello(sPub, esPub, sNonce) {
  return new TextEncoder().encode(`{"v":1,"s":"${b64(sPub)}","e":"${b64(esPub)}","n":"${b64(sNonce)}"}`);
}

/**
 * HKDF-SHA256(salt = cNonce || sNonce, ikm, info = LABEL || SHA-256(clientHello || serverHello))
 * → 64 bytes; returns the raw halves { c2s, s2c }.
 */
export async function keySchedule(ikm, cNonce, sNonce, clientHelloBytes, serverHelloBytes) {
  const info = concat(LABEL, await sha256(concat(clientHelloBytes, serverHelloBytes)));
  const base = await subtle.importKey('raw', ikm, 'HKDF', false, ['deriveBits']);
  const okm = new Uint8Array(
    await subtle.deriveBits({ name: 'HKDF', hash: 'SHA-256', salt: concat(cNonce, sNonce), info }, base, 512),
  );
  return { c2s: okm.slice(0, 32), s2c: okm.slice(32, 64) };
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

const MAX_CTR = (1n << 64n) - 1n;

/** One direction of a channel: AES-256-GCM with an implicit counter from 0. */
export async function directionKey(raw, dir) {
  const key = await subtle.importKey('raw', raw, 'AES-GCM', false, ['encrypt', 'decrypt']);
  return { key, dir, ctr: 0n };
}

async function step(state, op, data) {
  if (state.ctr >= MAX_CTR) throw new Error('counter exhausted');
  const params = { name: 'AES-GCM', iv: frameNonce(state.dir, state.ctr), additionalData: concat(LABEL, direction(state.dir)), tagLength: 128 };
  const out = new Uint8Array(await subtle[op](params, state.key, data));
  state.ctr += 1n;
  return out;
}

/** Encrypt the next frame: ciphertext then tag. */
export function seal(state, plaintext) {
  return step(state, 'encrypt', plaintext);
}

/** Decrypt the next frame; rejects on any failure and does not advance. */
export function open(state, wire) {
  return step(state, 'decrypt', wire);
}

// ---- the check ----

const V = JSON.parse(readFileSync(new URL('./e2e_vectors.json', import.meta.url), 'utf8'));
const unhex = (h) => Uint8Array.from(h.match(/../g) ?? [], (b) => parseInt(b, 16));
const hex = (bytes) => Buffer.from(bytes).toString('hex');
const utf8 = (bytes) => new TextDecoder().decode(bytes);

/** The public key of a scalar, derived outside WebCrypto. */
function publicOf(name) {
  const ecdhNode = createECDH('prime256v1');
  ecdhNode.setPrivateKey(Buffer.from(unhex(V[name])));
  return new Uint8Array(ecdhNode.getPublicKey(null, 'uncompressed'));
}

const pub = {
  s: publicOf('s_priv'),
  d: publicOf('d_priv'),
  ec: publicOf('ec_priv'),
  es: publicOf('es_priv'),
};
const cNonce = unhex(V.c_nonce);
const sNonce = unhex(V.s_nonce);

async function priv(name) {
  return importPrivate(unhex(V[`${name}_priv`]), pub[name]);
}

test('public keys and fingerprint follow from the scalars', async () => {
  assert.equal(hex(pub.s), V.s_pub);
  assert.equal(hex(pub.d), V.d_pub);
  assert.equal(await fp(pub.s), V.fp);
  assert.equal(V.fp.length, 43);
});

test('hellos are the exact bytes in the file', () => {
  assert.equal(utf8(clientHello(pub.d, pub.ec, cNonce)), V.client_hello);
  assert.equal(utf8(serverHello(pub.s, pub.es, sNonce)), V.server_hello);
});

test('the three ecdh outputs are the ikm, from both sides', async () => {
  const [d, ec, s, es] = await Promise.all(['d', 'ec', 's', 'es'].map(priv));
  const P = Object.fromEntries(await Promise.all(Object.entries(pub).map(async ([k, v]) => [k, await importPublic(v)])));
  const client = concat(await ecdh(ec, P.es), await ecdh(ec, P.s), await ecdh(d, P.es));
  const server = concat(await ecdh(es, P.ec), await ecdh(s, P.ec), await ecdh(es, P.d));
  assert.equal(hex(client), V.ikm);
  assert.equal(hex(server), V.ikm);
  // The JWK x/y came from node:crypto; node's own ECDH must agree, so a wrong
  // public key could not have slipped into the import.
  const node = (a, b) => {
    const e = createECDH('prime256v1');
    e.setPrivateKey(Buffer.from(unhex(V[`${a}_priv`])));
    return e.computeSecret(Buffer.from(pub[b]));
  };
  assert.equal(Buffer.concat([node('ec', 'es'), node('ec', 's'), node('d', 'es')]).toString('hex'), V.ikm);
});

test('hkdf gives the two direction keys', async () => {
  const keys = await keySchedule(unhex(V.ikm), cNonce, sNonce, new TextEncoder().encode(V.client_hello), new TextEncoder().encode(V.server_hello));
  assert.equal(hex(keys.c2s), V.k_c2s);
  assert.equal(hex(keys.s2c), V.k_s2c);
});

test('a hello differing in one whitespace byte gives other keys', async () => {
  const spaced = new TextEncoder().encode(V.client_hello.replace('{"v":1,', '{"v": 1,'));
  const keys = await keySchedule(unhex(V.ikm), cNonce, sNonce, spaced, new TextEncoder().encode(V.server_hello));
  assert.notEqual(hex(keys.c2s), V.k_c2s);
});

test('every frame seals to its wire bytes and opens back', async () => {
  for (const [name, dir] of [['c2s', C2S], ['s2c', S2C]]) {
    const raw = unhex(V[`k_${name}`]);
    const sender = await directionKey(raw, dir);
    const receiver = await directionKey(raw, dir);
    for (const frame of V.frames[name]) {
      const plain = new TextEncoder().encode(frame.plain);
      const wire = await seal(sender, plain);
      assert.equal(hex(wire), frame.wire, `${name} ${frame.plain}`);
      assert.equal(wire.length, plain.length + 16);
      assert.equal(utf8(await open(receiver, unhex(frame.wire))), frame.plain);
    }
  }
});

test('tampered, reordered or wrong-direction frames are rejected', async () => {
  const [first, second] = V.frames.c2s.map((f) => unhex(f.wire));
  const c2s = await directionKey(unhex(V.k_c2s), C2S);
  await assert.rejects(open(c2s, second), 'reorder');
  const bad = first.slice();
  bad[bad.length - 1] ^= 1;
  await assert.rejects(open(c2s, bad), 'tag bit flip');
  await assert.rejects(open(c2s, first.subarray(0, 15)), 'shorter than a tag');
  const flipped = await directionKey(unhex(V.k_c2s), S2C);
  await assert.rejects(open(flipped, first), 'wrong direction in nonce and aad');
  // Failures did not advance the counter.
  assert.equal(c2s.ctr, 0n);
  assert.equal(utf8(await open(c2s, first)), '');
  await assert.rejects(open(c2s, first), 'replay');
});

test('the counter stops before it would wrap', async () => {
  const state = await directionKey(unhex(V.k_c2s), C2S);
  state.ctr = MAX_CTR - 1n;
  await seal(state, new Uint8Array(1));
  assert.equal(state.ctr, MAX_CTR);
  await assert.rejects(seal(state, new Uint8Array(1)), /counter exhausted/);
});

test('base64url is strict', () => {
  assert.equal(b64(Uint8Array.of(0xfb, 0xff)), '-_8');
  assert.deepEqual(unb64('-_8'), Uint8Array.of(0xfb, 0xff));
  for (const bad of ['AA==', 'A', 'a+b/', 'AB']) assert.throws(() => unb64(bad), bad);
  assert.deepEqual(unb64(''), new Uint8Array(0));
});

test('an off-curve or compressed public key is refused', async () => {
  const off = concat(Uint8Array.of(4), new Uint8Array(64).fill(1));
  await assert.rejects(importPublic(off));
  assert.throws(() => importPublic(concat(Uint8Array.of(2), pub.s.subarray(1, 33))));
});
