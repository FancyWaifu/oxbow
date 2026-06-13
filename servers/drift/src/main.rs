//! drift — the DRIFT client for oxbow (start: an SSE crypto self-test).
//!
//! DRIFT is Bryson's encrypted, identity-based transport (pubkey = address).
//! Its handshake uses X25519, BLAKE2b, and ChaCha20-Poly1305 — SIMD crypto that
//! needs SSE, which `x86_64-unknown-none` ships disabled. This program is built
//! with hardware SSE (`-soft-float,+sse,+sse2`, see the justfile) and runs in
//! ring 3 on oxbow's just-added FPU/SSE support. This first cut proves the
//! crypto runs correctly on oxbow; the network handshake over the TCP socket
//! capability API comes next.
#![no_std]
#![no_main]

extern crate alloc;

use blake2::{Blake2b512, Digest};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use oxbow_rt as rt;
use x25519_dalek::{PublicKey, StaticSecret};

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    rt::println!("[drift] SSE crypto self-test (X25519 / BLAKE2b / ChaCha20-Poly1305)");

    // 1. X25519 Diffie-Hellman: two parties derive the same shared secret.
    //    (Deterministic seeds here — real key generation needs a CSPRNG, next arc.)
    let a = StaticSecret::from([0x11u8; 32]);
    let b = StaticSecret::from([0x22u8; 32]);
    let a_pub = PublicKey::from(&a);
    let b_pub = PublicKey::from(&b);
    let ss_ab = a.diffie_hellman(&b_pub);
    let ss_ba = b.diffie_hellman(&a_pub);
    let dh_ok = ss_ab.as_bytes() == ss_ba.as_bytes();
    let s = ss_ab.as_bytes();
    rt::println!(
        "[drift] X25519 DH agree: {}  shared={:02x}{:02x}{:02x}{:02x}..",
        dh_ok, s[0], s[1], s[2], s[3]
    );

    // 2. BLAKE2b session-key derivation (DRIFT's "drift-session-v2" KDF shape).
    let mut h = Blake2b512::new();
    h.update(b"drift-session-v2");
    h.update(ss_ab.as_bytes());
    let key = h.finalize();
    rt::println!("[drift] BLAKE2b session key={:02x}{:02x}{:02x}{:02x}..", key[0], key[1], key[2], key[3]);

    // 3. ChaCha20-Poly1305 AEAD round-trip with that key.
    let cipher = ChaCha20Poly1305::new_from_slice(&key[..32]).expect("key");
    let nonce = [0u8; 12];
    let plain = b"hello from oxbow over drift";
    let ct = cipher.encrypt((&nonce).into(), plain.as_slice()).expect("encrypt");
    let pt = cipher.decrypt((&nonce).into(), ct.as_slice()).expect("decrypt");
    let aead_ok = pt.as_slice() == plain.as_slice();
    rt::println!("[drift] ChaCha20-Poly1305 round-trip: {}  ciphertext {} bytes", aead_ok, ct.len());

    if dh_ok && aead_ok {
        rt::println!("[drift] OK — DRIFT crypto runs on oxbow (SSE + FPU context-switch)");
    } else {
        rt::println!("[drift] FAIL — crypto mismatch");
    }
    rt::sys_exit(if dh_ok && aead_ok { 0 } else { 1 });
}
