//! rng — a kernel CSPRNG (ChaCha20 with fast key erasure, the arc4random design),
//! seeded from the CPU hardware RNG (RDSEED, then RDRAND) with an RDTSC-mixing
//! fallback. It feeds stack-base ASLR at process load and the `getentropy`
//! syscall (which the libc arc4random + stack cookies draw from).
//!
//! Fast key erasure (Bernstein): each refill runs one ChaCha20 block; the first
//! 32 bytes of keystream OVERWRITE the key (so past output can't be recovered
//! from a later state capture — forward secrecy), the remaining 32 bytes are the
//! random output.
use spin::Mutex;

const CHACHA_CONST: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// One 64-byte ChaCha20 keystream block (20 rounds), nonce fixed at 0.
fn block(key: &[u32; 8], counter: u64, out: &mut [u8; 64]) {
    let mut s = [0u32; 16];
    s[0..4].copy_from_slice(&CHACHA_CONST);
    s[4..12].copy_from_slice(key);
    s[12] = counter as u32;
    s[13] = (counter >> 32) as u32;
    let init = s;
    for _ in 0..10 {
        qr(&mut s, 0, 4, 8, 12);
        qr(&mut s, 1, 5, 9, 13);
        qr(&mut s, 2, 6, 10, 14);
        qr(&mut s, 3, 7, 11, 15);
        qr(&mut s, 0, 5, 10, 15);
        qr(&mut s, 1, 6, 11, 12);
        qr(&mut s, 2, 7, 8, 13);
        qr(&mut s, 3, 4, 9, 14);
    }
    for i in 0..16 {
        let v = s[i].wrapping_add(init[i]);
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
}

// --- Hardware entropy --------------------------------------------------------
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    ((hi as u64) << 32) | (lo as u64)
}

fn has_feature(leaf: u32, sub: u32, reg: u8, bit: u32) -> bool {
    // reg: 1 = ecx, 2 = ebx
    let r = if sub == 0 && leaf == 1 {
        core::arch::x86_64::__cpuid(leaf)
    } else {
        core::arch::x86_64::__cpuid_count(leaf, sub)
    };
    let word = if reg == 2 { r.ebx } else { r.ecx };
    word & (1 << bit) != 0
}

/// Try the hardware RNG: RDSEED (true entropy) first, then RDRAND. `None` if
/// neither is present or both keep failing (CF=0).
fn hw_rand64() -> Option<u64> {
    unsafe {
        if has_feature(7, 0, 2, 18) {
            // RDSEED (CPUID.7.0:EBX.18)
            for _ in 0..32 {
                let v: u64;
                let ok: u8;
                core::arch::asm!("rdseed {}; setc {}", out(reg) v, out(reg_byte) ok, options(nomem, nostack));
                if ok != 0 {
                    return Some(v);
                }
            }
        }
        if has_feature(1, 0, 1, 30) {
            // RDRAND (CPUID.1:ECX.30)
            for _ in 0..32 {
                let v: u64;
                let ok: u8;
                core::arch::asm!("rdrand {}; setc {}", out(reg) v, out(reg_byte) ok, options(nomem, nostack));
                if ok != 0 {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// 256 bits of seed material, mixing the hardware RNG with timestamp jitter so we
/// degrade (weakly) rather than fail on a CPU without RDSEED/RDRAND.
fn hw_seed() -> [u32; 8] {
    let mut out = [0u32; 8];
    for (i, o) in out.iter_mut().enumerate() {
        let mut v = rdtsc().rotate_left((i as u32) * 7);
        if let Some(r) = hw_rand64() {
            v ^= r;
        }
        v ^= rdtsc();
        *o = (v ^ (v >> 32)) as u32;
    }
    out
}

struct Csprng {
    key: [u32; 8],
    counter: u64,
    seeded: bool,
}

impl Csprng {
    const fn new() -> Self {
        Csprng { key: [0; 8], counter: 0, seeded: false }
    }
    fn reseed(&mut self) {
        let s = hw_seed();
        for i in 0..8 {
            self.key[i] ^= s[i];
        }
        self.seeded = true;
    }
    fn fill(&mut self, buf: &mut [u8]) {
        if !self.seeded {
            self.reseed();
        }
        let mut off = 0;
        while off < buf.len() {
            let mut blk = [0u8; 64];
            block(&self.key, self.counter, &mut blk);
            self.counter = self.counter.wrapping_add(1);
            // Fast key erasure: first 32 bytes become the new key.
            for i in 0..8 {
                self.key[i] =
                    u32::from_le_bytes([blk[i * 4], blk[i * 4 + 1], blk[i * 4 + 2], blk[i * 4 + 3]]);
            }
            let n = core::cmp::min(32, buf.len() - off);
            buf[off..off + n].copy_from_slice(&blk[32..32 + n]);
            off += n;
        }
    }
}

static RNG: Mutex<Csprng> = Mutex::new(Csprng::new());

/// Seed the CSPRNG from hardware entropy. Call once early at boot.
pub fn init() {
    let mut rng = RNG.lock();
    rng.reseed();
    let src = if has_feature(7, 0, 2, 18) {
        "RDSEED"
    } else if has_feature(1, 0, 1, 30) {
        "RDRAND"
    } else {
        "RDTSC (no hw RNG!)"
    };
    drop(rng);
    crate::println!("[rng] CSPRNG seeded (ChaCha20, entropy: {})", src);
}

/// Fill `buf` with cryptographically-random bytes.
pub fn fill_bytes(buf: &mut [u8]) {
    RNG.lock().fill(buf);
}

/// A random u64.
pub fn next_u64() -> u64 {
    let mut b = [0u8; 8];
    fill_bytes(&mut b);
    u64::from_le_bytes(b)
}
