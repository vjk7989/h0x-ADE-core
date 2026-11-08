//! End-to-end encryption for the web panel's WebSocket (ADR-064). Pure
//! functions and values: no socket, no file I/O. A WebCrypto client must match
//! this byte for byte; `e2e_vectors.json` is the frozen contract.
//!
//! - Keys are P-256. A public key is SEC1 uncompressed (65 bytes). `b64` is
//!   base64url without padding. `fp = b64(SHA-256(S_pub))`; the client pins it.
//! - Client hello (UTF-8 JSON text): `{"v":1,"d":b64(D_pub),"e":b64(Ec_pub),"n":b64(32 random bytes)}`.
//!   Server hello: `{"v":1,"s":b64(S_pub),"e":b64(Es_pub),"n":b64(32 random bytes)}`.
//!   `transcript` = the exact client hello bytes then the exact server hello bytes.
//!   Unknown hello fields are accepted on purpose (the exact bytes are in the
//!   transcript); duplicate fields are rejected. A reformatted hello is caught at
//!   the first frame, so the caller must close on the first `open` error.
//! - The client checks `b64(SHA-256(S_pub)) == pinned fp` (constant time).
//! - `ikm = ECDH(Ec,Es) || ECDH(Ec,S) || ECDH(D,Es)` (each the 32-byte x coordinate);
//!   the server computes the mirror `ECDH(Es,Ec) || ECDH(S,Ec) || ECDH(Es,D)`.
//! - `HKDF-SHA256(salt = c_nonce || s_nonce, ikm, info = "h0x-bridge v1" || SHA-256(transcript))`
//!   gives 64 bytes: `k_c2s = [0..32]`, `k_s2c = [32..64]`.
//! - Frames: AES-256-GCM. Nonce = 4-byte big-endian direction (c2s = 1, s2c = 2)
//!   then an 8-byte big-endian counter, starting at 0 per direction, implicit
//!   (never sent), +1 per frame. AAD = `"h0x-bridge v1"` then the 4 direction
//!   bytes. Wire payload = ciphertext then tag. Any open failure is an error and
//!   the caller must close the connection. The counter never wraps or repeats.

// Wired into the connection by SEC-2b.
#![allow(dead_code)]

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use hkdf::Hkdf;
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::Generate;
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::{PublicKey, SecretKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt;

const LABEL: &[u8] = b"h0x-bridge v1";
const C2S: u32 = 1;
const S2C: u32 = 2;
const PUB_LEN: usize = 65;
const NONCE_LEN: usize = 32;

/// Why a handshake or a frame was refused. The caller closes the connection.
#[derive(Debug, PartialEq, Eq)]
pub struct Error(&'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// base64url, no padding.
pub fn b64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Strict inverse of [`b64`]: only the URL alphabet, no padding, canonical.
fn unb64(text: &str) -> Result<Vec<u8>> {
    let bad = Error("bad base64");
    if text.len() % 4 == 1 {
        return Err(bad);
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(bad),
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero so that one byte string has one encoding.
    if acc != 0 { Err(bad) } else { Ok(out) }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn public_bytes(key: &SecretKey) -> [u8; PUB_LEN] {
    let point = key.public_key().to_sec1_point(false);
    point.as_bytes().try_into().expect("uncompressed P-256 point is 65 bytes")
}

/// Parse a 65-byte uncompressed public key; rejects points not on the curve.
fn parse_public(bytes: &[u8]) -> Result<PublicKey> {
    if bytes.len() != PUB_LEN || bytes[0] != 4 {
        return Err(Error("bad public key"));
    }
    PublicKey::from_sec1_bytes(bytes).map_err(|_| Error("bad public key"))
}

fn ecdh(secret: &SecretKey, public: &PublicKey) -> [u8; 32] {
    let shared = diffie_hellman(secret.to_nonzero_scalar(), public.as_affine());
    let mut out = [0u8; 32];
    out.copy_from_slice(shared.raw_secret_bytes());
    out
}

fn random_nonce() -> [u8; NONCE_LEN] {
    <[u8; NONCE_LEN]>::generate()
}

/// Exactly 32 bytes: `from_slice` would left-pad a truncated key into a different one.
fn secret_key(bytes: &[u8]) -> Result<SecretKey> {
    if bytes.len() != 32 {
        return Err(Error("bad secret key"));
    }
    SecretKey::from_slice(bytes).map_err(|_| Error("bad secret key"))
}

/// The server's long-term identity key S.
pub struct ServerKey(SecretKey);

impl ServerKey {
    pub fn generate() -> Self {
        Self(SecretKey::generate())
    }

    pub fn from_bytes(secret: &[u8]) -> Result<Self> {
        secret_key(secret).map(Self)
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.0.to_bytes());
        out
    }

    pub fn public(&self) -> [u8; PUB_LEN] {
        public_bytes(&self.0)
    }

    /// `b64(SHA-256(S_pub))`, the value carried by the pairing link.
    pub fn fp(&self) -> String {
        fp(&self.public())
    }
}

pub fn fp(server_public: &[u8]) -> String {
    b64(&sha256(server_public))
}

#[derive(Deserialize)]
struct ClientHello {
    v: u64,
    d: String,
    e: String,
    n: String,
}

#[derive(Deserialize)]
struct ServerHello {
    v: u64,
    s: String,
    e: String,
    n: String,
}

fn field(text: &str, len: usize) -> Result<Vec<u8>> {
    let bytes = unb64(text)?;
    if bytes.len() == len { Ok(bytes) } else { Err(Error("bad length")) }
}

/// Both sides' derivation. `ikm` is the three ECDH outputs in protocol order.
fn channel(
    ikm: [[u8; 32]; 3],
    c_nonce: &[u8],
    s_nonce: &[u8],
    client_hello: &[u8],
    server_hello: &[u8],
    server: bool,
) -> Channel {
    let mut transcript = client_hello.to_vec();
    transcript.extend_from_slice(server_hello);
    let mut info = LABEL.to_vec();
    info.extend_from_slice(&sha256(&transcript));
    let salt = [c_nonce, s_nonce].concat();
    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm.concat());
    let mut okm = [0u8; 64];
    hk.expand(&info, &mut okm).expect("64 bytes is a valid HKDF-SHA256 length");
    let key = |half: &[u8]| Aes256Gcm::new_from_slice(half).expect("32-byte key");
    let (c2s, s2c) = (key(&okm[..32]), key(&okm[32..]));
    let (send, send_dir, recv, recv_dir) = if server { (s2c, S2C, c2s, C2S) } else { (c2s, C2S, s2c, S2C) };
    Channel { send, recv, send_dir, recv_dir, send_ctr: 0, recv_ctr: 0 }
}

/// What the server gets from a good client hello.
pub struct Accepted {
    /// Send this text back to the client.
    pub server_hello: Vec<u8>,
    pub channel: Channel,
    /// The client's device key D_pub (65 bytes); auth must be bound to it.
    pub device: [u8; PUB_LEN],
}

/// Server side: validate the client hello and answer with a fresh ephemeral.
pub fn server_accept(client_hello: &[u8], key: &ServerKey) -> Result<Accepted> {
    accept(client_hello, key, SecretKey::generate(), random_nonce())
}

fn accept(client_hello: &[u8], key: &ServerKey, es: SecretKey, s_nonce: [u8; NONCE_LEN]) -> Result<Accepted> {
    let text = std::str::from_utf8(client_hello).map_err(|_| Error("hello is not utf-8"))?;
    let hello: ClientHello = serde_json::from_str(text).map_err(|_| Error("bad hello"))?;
    if hello.v != 1 {
        return Err(Error("unsupported version"));
    }
    let d_bytes = field(&hello.d, PUB_LEN)?;
    let device = parse_public(&d_bytes)?;
    let ec = parse_public(&field(&hello.e, PUB_LEN)?)?;
    let c_nonce = field(&hello.n, NONCE_LEN)?;
    let server_hello =
        format!(r#"{{"v":1,"s":"{}","e":"{}","n":"{}"}}"#, b64(&key.public()), b64(&public_bytes(&es)), b64(&s_nonce))
            .into_bytes();
    let ikm = [ecdh(&es, &ec), ecdh(&key.0, &ec), ecdh(&es, &device)];
    let channel = channel(ikm, &c_nonce, &s_nonce, client_hello, &server_hello, true);
    let device = d_bytes.try_into().expect("length checked");
    Ok(Accepted { server_hello, channel, device })
}

/// Client side, after sending the hello and before the server's answer.
pub struct ClientPending {
    d: SecretKey,
    ec: SecretKey,
    c_nonce: [u8; NONCE_LEN],
    hello: Vec<u8>,
}

/// Client side: build the hello from the long-term device key `D` (32-byte secret).
pub fn client_start(device_secret: &[u8]) -> Result<ClientPending> {
    let d = secret_key(device_secret)?;
    Ok(start(d, SecretKey::generate(), random_nonce()))
}

fn start(d: SecretKey, ec: SecretKey, c_nonce: [u8; NONCE_LEN]) -> ClientPending {
    let hello = format!(
        r#"{{"v":1,"d":"{}","e":"{}","n":"{}"}}"#,
        b64(&public_bytes(&d)),
        b64(&public_bytes(&ec)),
        b64(&c_nonce)
    )
    .into_bytes();
    ClientPending { d, ec, c_nonce, hello }
}

impl ClientPending {
    /// The text to send first.
    pub fn hello(&self) -> &[u8] {
        &self.hello
    }

    /// Check the server hello against the pinned fingerprint and derive the channel.
    pub fn finish(self, server_hello: &[u8], pinned_fp: &str) -> Result<Channel> {
        let text = std::str::from_utf8(server_hello).map_err(|_| Error("hello is not utf-8"))?;
        let hello: ServerHello = serde_json::from_str(text).map_err(|_| Error("bad hello"))?;
        if hello.v != 1 {
            return Err(Error("unsupported version"));
        }
        let s_bytes = field(&hello.s, PUB_LEN)?;
        let s = parse_public(&s_bytes)?;
        let es = parse_public(&field(&hello.e, PUB_LEN)?)?;
        let s_nonce = field(&hello.n, NONCE_LEN)?;
        let got = fp(&s_bytes);
        if !constant_time_eq(got.as_bytes(), pinned_fp.as_bytes()) {
            return Err(Error("server fingerprint mismatch"));
        }
        let ikm = [ecdh(&self.ec, &es), ecdh(&self.ec, &s), ecdh(&self.d, &es)];
        Ok(channel(ikm, &self.c_nonce, &s_nonce, &self.hello, server_hello, false))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One established connection: a key and an implicit counter per direction.
pub struct Channel {
    send: Aes256Gcm,
    recv: Aes256Gcm,
    send_dir: u32,
    recv_dir: u32,
    send_ctr: u64,
    recv_ctr: u64,
}

fn frame_nonce(dir: u32, ctr: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&dir.to_be_bytes());
    nonce[4..].copy_from_slice(&ctr.to_be_bytes());
    nonce
}

fn frame_aad(dir: u32) -> Vec<u8> {
    [LABEL, &dir.to_be_bytes()].concat()
}

impl Channel {
    /// Encrypt the next frame; the result is ciphertext then tag.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let ctr = self.send_ctr;
        let next = ctr.checked_add(1).ok_or(Error("counter exhausted"))?;
        let nonce = frame_nonce(self.send_dir, ctr);
        let aad = frame_aad(self.send_dir);
        let sealed = self
            .send
            .encrypt((&nonce).into(), Payload { msg: plaintext, aad: &aad })
            .map_err(|_| Error("seal failed"))?;
        self.send_ctr = next;
        Ok(sealed)
    }

    /// Decrypt the next frame. Any error means: close the connection.
    pub fn open(&mut self, wire: &[u8]) -> Result<Vec<u8>> {
        let ctr = self.recv_ctr;
        let next = ctr.checked_add(1).ok_or(Error("counter exhausted"))?;
        let nonce = frame_nonce(self.recv_dir, ctr);
        let aad = frame_aad(self.recv_dir);
        let plain =
            self.recv.decrypt((&nonce).into(), Payload { msg: wire, aad: &aad }).map_err(|_| Error("open failed"))?;
        self.recv_ctr = next;
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    fn pair() -> (ServerKey, Accepted, Channel) {
        let server = ServerKey::generate();
        let device = SecretKey::generate();
        let pending = client_start(&device.to_bytes()).unwrap();
        let accepted = server_accept(pending.hello(), &server).unwrap();
        let client = pending.finish(&accepted.server_hello, &server.fp()).unwrap();
        (server, accepted, client)
    }

    #[test]
    fn base64url_round_trips_and_is_strict() {
        for n in 0..8usize {
            let bytes: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(97).wrapping_add(250)).collect();
            let text = b64(&bytes);
            assert!(!text.contains(['=', '+', '/']));
            assert_eq!(unb64(&text).unwrap(), bytes);
        }
        assert_eq!(b64(&[0xfb, 0xff]), "-_8");
        assert!(unb64("AA==").is_err());
        assert!(unb64("A").is_err());
        assert!(unb64("a+b/").is_err());
        assert!(unb64("AB").is_err(), "non-canonical trailing bits");
    }

    #[test]
    fn round_trip_both_directions() {
        let (_, mut accepted, mut client) = pair();
        for i in 0..3u8 {
            let up = client.seal(&[i, 1, 2]).unwrap();
            assert_eq!(accepted.channel.open(&up).unwrap(), vec![i, 1, 2]);
            let down = accepted.channel.seal(&[9, i]).unwrap();
            assert_eq!(client.open(&down).unwrap(), vec![9, i]);
        }
    }

    #[test]
    fn empty_frame_works() {
        let (_, mut accepted, mut client) = pair();
        let wire = client.seal(b"").unwrap();
        assert_eq!(wire.len(), 16, "tag only");
        assert_eq!(accepted.channel.open(&wire).unwrap(), b"");
    }

    #[test]
    fn exposes_the_device_key() {
        let server = ServerKey::generate();
        let device = SecretKey::generate();
        let pending = client_start(&device.to_bytes()).unwrap();
        let accepted = server_accept(pending.hello(), &server).unwrap();
        assert_eq!(accepted.device, public_bytes(&device));
    }

    #[test]
    fn tampering_is_an_error() {
        let (_, mut accepted, mut client) = pair();
        let wire = client.seal(b"hello").unwrap();
        for i in 0..wire.len() {
            let mut bad = wire.clone();
            bad[i] ^= 1;
            assert!(accepted.channel.open(&bad).is_err(), "byte {i}");
        }
        assert!(accepted.channel.open(&wire[..wire.len() - 1]).is_err());
        assert!(accepted.channel.open(&[]).is_err());
        // Failed opens do not advance the counter: the real frame still opens.
        assert_eq!(accepted.channel.open(&wire).unwrap(), b"hello");
    }

    #[test]
    fn replay_and_reorder_are_errors() {
        let (_, mut accepted, mut client) = pair();
        let first = client.seal(b"one").unwrap();
        let second = client.seal(b"two").unwrap();
        assert!(accepted.channel.open(&second).is_err(), "reorder");
        accepted.channel.open(&first).unwrap();
        assert!(accepted.channel.open(&first).is_err(), "replay");
        accepted.channel.open(&second).unwrap();
    }

    #[test]
    fn wrong_direction_is_an_error() {
        let (_, mut accepted, mut client) = pair();
        let up = client.seal(b"up").unwrap();
        assert!(client.open(&up).is_err(), "own frame reflected back");
        let down = accepted.channel.seal(b"down").unwrap();
        assert!(accepted.channel.open(&down).is_err());
    }

    #[test]
    fn counter_exhaustion_is_an_error() {
        let (_, _, mut client) = pair();
        client.send_ctr = u64::MAX;
        assert!(client.seal(b"x").is_err());
        client.recv_ctr = u64::MAX;
        assert!(client.open(&[0; 16]).is_err());
    }

    #[test]
    fn wrong_fingerprint_is_an_error() {
        let server = ServerKey::generate();
        let other = ServerKey::generate();
        let pending = client_start(&SecretKey::generate().to_bytes()).unwrap();
        let accepted = server_accept(pending.hello(), &server).unwrap();
        assert!(pending.finish(&accepted.server_hello, &other.fp()).is_err());
    }

    #[test]
    fn other_server_key_fails_even_with_right_fp_text() {
        let server = ServerKey::generate();
        let impostor = ServerKey::generate();
        let pending = client_start(&SecretKey::generate().to_bytes()).unwrap();
        let accepted = server_accept(pending.hello(), &impostor).unwrap();
        assert!(pending.finish(&accepted.server_hello, &server.fp()).is_err());
        assert!(client_start(&[0; 32]).is_err(), "zero is not a key");
        assert!(client_start(&[1; 5]).is_err());
    }

    #[test]
    fn bad_client_hello_is_an_error() {
        let server = ServerKey::generate();
        let good = client_start(&SecretKey::generate().to_bytes()).unwrap();
        let text = String::from_utf8(good.hello().to_vec()).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        let with = |key: &str, value: Value| {
            let mut v = parsed.clone();
            v[key] = value;
            v.to_string().into_bytes()
        };
        let short_key = b64(&[4; 64]);
        let off_curve = b64(&[&[4u8][..], &[1u8; 64]].concat());
        let not_uncompressed = b64(&[&[2u8][..], &[1u8; 64]].concat());
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"{}".to_vec(),
            b"not json".to_vec(),
            vec![0xff, 0xfe],
            with("v", 2.into()),
            with("v", 0.into()),
            with("d", short_key.clone().into()),
            with("e", short_key.into()),
            with("d", off_curve.clone().into()),
            with("e", off_curve.into()),
            with("e", not_uncompressed.into()),
            with("d", "***".into()),
            with("n", b64(&[0; 31]).into()),
            with("n", Value::Null),
        ];
        for (i, case) in cases.iter().enumerate() {
            assert!(server_accept(case, &server).is_err(), "case {i}");
        }
    }

    #[test]
    fn bad_server_hello_is_an_error() {
        let server = ServerKey::generate();
        let device = SecretKey::generate().to_bytes();
        let pending = client_start(&device).unwrap();
        let accepted = server_accept(pending.hello(), &server).unwrap();
        let parsed: Value = serde_json::from_slice(&accepted.server_hello).unwrap();
        let fp = server.fp();
        let off_curve = b64(&[&[4u8][..], &[1u8; 64]].concat());
        let mut cases: Vec<Vec<u8>> = vec![Vec::new(), b"{}".to_vec(), vec![0xff]];
        for (key, value) in [
            ("v", Value::from(2)),
            ("s", off_curve.clone().into()),
            ("e", off_curve.into()),
            ("e", b64(&[4; 10]).into()),
            ("n", b64(&[0; 33]).into()),
            ("n", "!".into()),
        ] {
            let mut v = parsed.clone();
            v[key] = value;
            cases.push(v.to_string().into_bytes());
        }
        for (i, case) in cases.iter().enumerate() {
            let pending = client_start(&device).unwrap();
            assert!(pending.finish(case, &fp).is_err(), "case {i}");
        }
    }

    #[test]
    fn server_key_round_trips_and_fp_is_stable() {
        let key = ServerKey::generate();
        let again = ServerKey::from_bytes(&key.to_bytes()).unwrap();
        assert_eq!(key.public(), again.public());
        assert_eq!(key.fp(), again.fp());
        assert_eq!(key.fp().len(), 43);
        assert!(ServerKey::from_bytes(&[0; 32]).is_err());
        assert!(ServerKey::from_bytes(&[]).is_err());
        for len in [0, 31, 33] {
            assert!(ServerKey::from_bytes(&vec![7; len]).is_err(), "server {len}");
            assert!(client_start(&vec![7; len]).is_err(), "device {len}");
        }
    }

    /// The frozen contract the browser client tests against (SEC-3): every
    /// value is recomputed from the fixed inputs.
    #[test]
    fn vectors_are_reproduced() {
        let v: Value = serde_json::from_str(include_str!("e2e_vectors.json")).unwrap();
        let get = |k: &str| v[k].as_str().unwrap_or_else(|| panic!("missing {k}")).to_string();
        let key = |k: &str| SecretKey::from_slice(&unhex(&get(k))).unwrap();
        let nonce = |k: &str| <[u8; NONCE_LEN]>::try_from(unhex(&get(k))).unwrap();
        let server = ServerKey(key("s_priv"));
        assert_eq!(hex(&server.public()), get("s_pub"));
        assert_eq!(server.fp(), get("fp"));
        let pending = start(key("d_priv"), key("ec_priv"), nonce("c_nonce"));
        assert_eq!(hex(&public_bytes(&key("d_priv"))), get("d_pub"));
        assert_eq!(String::from_utf8(pending.hello().to_vec()).unwrap(), get("client_hello"));
        let accepted = accept(pending.hello(), &server, key("es_priv"), nonce("s_nonce")).unwrap();
        assert_eq!(String::from_utf8(accepted.server_hello.clone()).unwrap(), get("server_hello"));
        let mut server_side = accepted.channel;
        let mut client_side = pending.finish(&accepted.server_hello, &get("fp")).unwrap();

        // Keys and ikm, recomputed independently of `channel`.
        let ec = key("ec_priv");
        let (es, s, d) = (key("es_priv"), key("s_priv"), key("d_priv"));
        let ikm = [ecdh(&ec, &es.public_key()), ecdh(&ec, &s.public_key()), ecdh(&d, &es.public_key())].concat();
        assert_eq!(hex(&ikm), get("ikm"));
        let transcript = [get("client_hello"), get("server_hello")].concat();
        let mut info = LABEL.to_vec();
        info.extend_from_slice(&sha256(transcript.as_bytes()));
        let salt = [unhex(&get("c_nonce")), unhex(&get("s_nonce"))].concat();
        let mut okm = [0u8; 64];
        Hkdf::<Sha256>::new(Some(&salt), &ikm).expand(&info, &mut okm).unwrap();
        assert_eq!(hex(&okm[..32]), get("k_c2s"));
        assert_eq!(hex(&okm[32..]), get("k_s2c"));

        for dir in ["c2s", "s2c"] {
            for frame in v["frames"][dir].as_array().unwrap() {
                let plain = frame["plain"].as_str().unwrap();
                let (sender, receiver) = if dir == "c2s" {
                    (&mut client_side, &mut server_side)
                } else {
                    (&mut server_side, &mut client_side)
                };
                let wire = sender.seal(plain.as_bytes()).unwrap();
                assert_eq!(hex(&wire), frame["wire"].as_str().unwrap(), "{dir} {plain}");
                assert_eq!(receiver.open(&wire).unwrap(), plain.as_bytes());
            }
        }
    }

    /// Unknown fields and other whitespace are accepted: the transcript is the
    /// exact received bytes, so both sides still bind the same text.
    #[test]
    fn hello_with_extra_field_and_whitespace_still_agrees() {
        let server = ServerKey::generate();
        let mut pending = client_start(&SecretKey::generate().to_bytes()).unwrap();
        let text = String::from_utf8(pending.hello.clone()).unwrap();
        pending.hello =
            format!(" {}, \"x\": [1, {{}}] }}\n", text.trim_end_matches('}').replace(",", " ,\t")).into_bytes();
        let mut accepted = server_accept(&pending.hello, &server).unwrap();
        let mut client = pending.finish(&accepted.server_hello, &server.fp()).unwrap();
        assert_eq!(accepted.channel.open(&client.seal(b"up").unwrap()).unwrap(), b"up");
        assert_eq!(client.open(&accepted.channel.seal(b"down").unwrap()).unwrap(), b"down");
    }

    #[test]
    fn hello_reformatted_in_transit_breaks_the_channel() {
        let server = ServerKey::generate();
        let pending = client_start(&SecretKey::generate().to_bytes()).unwrap();
        let sent = String::from_utf8(pending.hello().to_vec()).unwrap();
        let received = sent.replace("\"v\":1", "\"v\": 1");
        let mut accepted = server_accept(received.as_bytes(), &server).unwrap();
        let mut client = pending.finish(&accepted.server_hello, &server.fp()).unwrap();
        assert!(accepted.channel.open(&client.seal(b"up").unwrap()).is_err());
        let pending = client_start(&SecretKey::generate().to_bytes()).unwrap();
        let mut accepted = server_accept(pending.hello(), &server).unwrap();
        let server_hello = String::from_utf8(accepted.server_hello.clone()).unwrap().replace(',', ", ");
        let mut client = pending.finish(server_hello.as_bytes(), &server.fp()).unwrap();
        assert!(client.open(&accepted.channel.seal(b"down").unwrap()).is_err());
    }

    #[test]
    fn duplicate_hello_keys_are_rejected() {
        let server = ServerKey::generate();
        let pending = client_start(&SecretKey::generate().to_bytes()).unwrap();
        let text = String::from_utf8(pending.hello().to_vec()).unwrap();
        let other_e = b64(&public_bytes(&SecretKey::generate()));
        let dup = format!("{},\"e\":\"{other_e}\"}}", text.trim_end_matches('}'));
        assert!(server_accept(dup.as_bytes(), &server).is_err());
        let accepted = server_accept(pending.hello(), &server).unwrap();
        let reply = String::from_utf8(accepted.server_hello).unwrap();
        let dup = format!("{},\"s\":\"{}\"}}", reply.trim_end_matches('}'), b64(&server.public()));
        assert!(pending.finish(dup.as_bytes(), &server.fp()).is_err());
    }

    #[test]
    fn identity_point_and_out_of_range_scalars_are_rejected() {
        let server = ServerKey::generate();
        let pending = client_start(&SecretKey::generate().to_bytes()).unwrap();
        let mut hello: Value = serde_json::from_slice(pending.hello()).unwrap();
        hello["e"] = b64(&[&[4u8][..], &[0u8; 64]].concat()).into();
        assert!(server_accept(hello.to_string().as_bytes(), &server).is_err(), "all-zero point");
        hello["e"] = b64(&[0u8]).into();
        assert!(server_accept(hello.to_string().as_bytes(), &server).is_err(), "sec1 identity encoding");
        // The P-256 group order n is not a key; n - 1 is the largest one.
        let n = unhex("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551");
        let mut n_minus_1 = n.clone();
        n_minus_1[31] -= 1;
        assert!(ServerKey::from_bytes(&n).is_err());
        assert!(ServerKey::from_bytes(&[0xff; 32]).is_err());
        assert!(client_start(&n).is_err());
        assert!(client_start(&[0xff; 33]).is_err());
        let top = ServerKey::from_bytes(&n_minus_1).unwrap();
        assert_eq!(top.to_bytes().to_vec(), n_minus_1);
        let mut one = [0u8; 32];
        one[31] = 1;
        assert!(client_start(&one).is_ok());
    }

    #[test]
    fn same_keys_with_other_nonces_derive_other_channels() {
        let server = ServerKey::from_bytes(&[0x11; 32]).unwrap();
        let fixed = |b: u8| SecretKey::from_slice(&[b; 32]).unwrap();
        let first_frame = |c_nonce: [u8; NONCE_LEN], s_nonce: [u8; NONCE_LEN]| {
            let pending = start(fixed(0x22), fixed(0x33), c_nonce);
            let accepted = accept(pending.hello(), &server, fixed(0x44), s_nonce).unwrap();
            pending.finish(&accepted.server_hello, &server.fp()).unwrap().seal(b"same").unwrap()
        };
        let base = first_frame([5; NONCE_LEN], [6; NONCE_LEN]);
        assert_eq!(base, first_frame([5; NONCE_LEN], [6; NONCE_LEN]), "deterministic for fixed inputs");
        assert_ne!(base, first_frame([7; NONCE_LEN], [6; NONCE_LEN]), "client nonce");
        assert_ne!(base, first_frame([5; NONCE_LEN], [7; NONCE_LEN]), "server nonce");
    }

    #[test]
    fn one_mebibyte_frame_round_trips() {
        let (_, mut accepted, mut client) = pair();
        let big: Vec<u8> = (0..1 << 20).map(|i: u32| (i % 251) as u8).collect();
        let wire = client.seal(&big).unwrap();
        assert_eq!(wire.len(), big.len() + 16);
        assert_eq!(accepted.channel.open(&wire).unwrap(), big);
        let down = accepted.channel.seal(&big).unwrap();
        assert_eq!(client.open(&down).unwrap(), big);
    }
}
