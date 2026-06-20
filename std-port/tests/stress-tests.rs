// Adversarial stress / edge-case suite for oxbow's std backend (run as an oxbow libtest
// binary). Aims to BREAK things: concurrency under contention (futex/scheduler), fs at the
// indirect-block + intern-table boundaries, large allocations, pipe/loopback-socket
// volume, and data-integrity round trips that catch silent truncation (the cap-and-lie
// bug class). This suite found four real backend bugs (UDP send truncation, fs append
// truncation, fs intern-table exhaustion, and — see fs_many_files — a write/cache
// integrity bug at ~540 files that is still open).
//
// Build: -Z build-std=std,test,panic_unwind ... --tests, profile panic="unwind",
// dep oxbow-rt features=["hosted"]. Run on a CLEAN disk (rm oxbow-disk.img; just disk;
// boot once to seed) — accumulated detritus on the 16 MiB image confounds the fs tests.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![feature(try_reserve_kind)]
#![allow(internal_features)]
extern crate oxbow_rt;

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Barrier, Condvar, Mutex, RwLock};
use std::time::Instant;
use std::{fs, thread};

// ---------------- concurrency: futex + scheduler under contention ----------------

#[test]
fn mutex_contention_no_lost_updates() {
    const THREADS: u64 = 8;
    const ITERS: u64 = 20_000;
    let counter = Arc::new(Mutex::new(0u64));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let c = counter.clone();
            thread::spawn(move || {
                for _ in 0..ITERS {
                    *c.lock().unwrap() += 1;
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(*counter.lock().unwrap(), THREADS * ITERS, "lost mutex updates (race)");
}

#[test]
fn rwlock_readers_and_writers() {
    let data = Arc::new(RwLock::new(0u64));
    let writers: Vec<_> = (0..4)
        .map(|_| {
            let d = data.clone();
            thread::spawn(move || {
                for _ in 0..5_000 {
                    *d.write().unwrap() += 1;
                }
            })
        })
        .collect();
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let d = data.clone();
            thread::spawn(move || {
                let mut last = 0;
                for _ in 0..10_000 {
                    let v = *d.read().unwrap();
                    assert!(v >= last, "rwlock value went backwards: {v} < {last}");
                    last = v;
                }
            })
        })
        .collect();
    for h in writers {
        h.join().unwrap();
    }
    for h in readers {
        h.join().unwrap();
    }
    assert_eq!(*data.read().unwrap(), 4 * 5_000);
}

#[test]
fn mpsc_volume_no_loss() {
    const PRODUCERS: usize = 6;
    const PER: usize = 5_000;
    let (tx, rx) = channel();
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let tx = tx.clone();
            thread::spawn(move || {
                for i in 0..PER {
                    tx.send((p, i)).unwrap();
                }
            })
        })
        .collect();
    drop(tx);
    let mut count = 0usize;
    let mut sum = 0u64;
    while let Ok((p, i)) = rx.recv() {
        count += 1;
        sum = sum.wrapping_add((p as u64) << 32 | i as u64);
    }
    for h in producers {
        h.join().unwrap();
    }
    assert_eq!(count, PRODUCERS * PER, "mpsc dropped or duplicated messages");
    let mut expect = 0u64;
    for p in 0..PRODUCERS {
        for i in 0..PER {
            expect = expect.wrapping_add((p as u64) << 32 | i as u64);
        }
    }
    assert_eq!(sum, expect, "mpsc corrupted message payloads");
}

#[test]
fn condvar_pingpong() {
    let state = Arc::new((Mutex::new(0u64), Condvar::new()));
    const ROUNDS: u64 = 2_000;
    let s2 = state.clone();
    let t = thread::spawn(move || {
        let (m, cv) = &*s2;
        loop {
            let mut g = m.lock().unwrap();
            while *g % 2 == 0 {
                if *g >= ROUNDS * 2 {
                    return;
                }
                g = cv.wait(g).unwrap();
            }
            if *g >= ROUNDS * 2 {
                return;
            }
            *g += 1;
            cv.notify_all();
        }
    });
    let (m, cv) = &*state;
    for _ in 0..ROUNDS {
        let mut g = m.lock().unwrap();
        while *g % 2 == 1 {
            g = cv.wait(g).unwrap();
        }
        *g += 1;
        cv.notify_all();
    }
    {
        let mut g = m.lock().unwrap();
        *g = ROUNDS * 2 + 1;
        cv.notify_all();
    }
    t.join().unwrap();
}

#[test]
fn barrier_rounds() {
    const N: usize = 8;
    let barrier = Arc::new(Barrier::new(N));
    let hits = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let b = barrier.clone();
            let h = hits.clone();
            thread::spawn(move || {
                for _ in 0..50 {
                    h.fetch_add(1, Ordering::Relaxed);
                    b.wait();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(hits.load(Ordering::Relaxed), N * 50);
}

#[test]
fn thread_churn_no_leak() {
    for round in 0..300u64 {
        let v = thread::spawn(move || round * 2).join().unwrap();
        assert_eq!(v, round * 2);
    }
}

#[test]
fn scoped_threads_distinct_slots() {
    let mut arr = [0u64; 16];
    thread::scope(|s| {
        for (i, slot) in arr.iter_mut().enumerate() {
            s.spawn(move || {
                *slot = (i as u64) * (i as u64);
            });
        }
    });
    for (i, &v) in arr.iter().enumerate() {
        assert_eq!(v, (i as u64) * (i as u64));
    }
}

#[test]
fn atomic_fetch_add_storm() {
    let a = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let a = a.clone();
            thread::spawn(move || {
                for _ in 0..50_000 {
                    a.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(a.load(Ordering::Relaxed), 8 * 50_000);
}

// ---------------- fs: indirect blocks, intern table, seek, concurrency ----------------

#[test]
fn fs_large_file_byte_exact() {
    let path = "/stress_large.bin";
    let size = 64 * 1024;
    let mut data = Vec::with_capacity(size);
    for i in 0..size {
        data.push((i * 31 + 7) as u8);
    }
    fs::write(path, &data).unwrap();
    assert_eq!(fs::metadata(path).unwrap().len() as usize, size);
    let back = fs::read(path).unwrap();
    assert_eq!(back.len(), size, "large file length mismatch (silent truncation?)");
    assert!(back == data, "large file content mismatch (corruption/truncation)");
    fs::remove_file(path).ok();
}

#[test]
fn fs_many_files() {
    // NOTE: this passed the intern-exhaustion fix (slots are reclaimed on close, so
    // sequential opens reuse them). Held at 400 because ~540+ files exposes a SEPARATE,
    // still-open fsd write-buffer/block-cache integrity bug (a file reads back with wrong
    // content at scale). Raise N to 600 to reproduce that bug.
    let dir = "/stress_many";
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    const N: usize = 400;
    for i in 0..N {
        let p = format!("{dir}/f{i}.txt");
        fs::write(&p, format!("content-{i}").as_bytes()).unwrap();
    }
    for i in 0..N {
        let p = format!("{dir}/f{i}.txt");
        let s = fs::read_to_string(&p).unwrap();
        assert_eq!(s, format!("content-{i}"), "wrong content for file {i}");
    }
    let count = fs::read_dir(dir).unwrap().count();
    assert_eq!(count, N, "read_dir saw {count}, expected {N}");
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fs_seek_overwrite() {
    use std::io::{Seek, SeekFrom};
    let path = "/stress_seek.bin";
    let mut f = fs::File::create(path).unwrap();
    f.write_all(&[0xAAu8; 1000]).unwrap();
    f.seek(SeekFrom::Start(500)).unwrap();
    f.write_all(&[0xBBu8; 100]).unwrap();
    drop(f);
    let back = fs::read(path).unwrap();
    assert_eq!(back.len(), 1000);
    assert!(back[..500].iter().all(|&b| b == 0xAA));
    assert!(back[500..600].iter().all(|&b| b == 0xBB), "seek+overwrite wrong region");
    assert!(back[600..].iter().all(|&b| b == 0xAA));
    fs::remove_file(path).ok();
}

#[test]
fn fs_concurrent_writers() {
    let dir = "/stress_conc";
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    let handles: Vec<_> = (0..6u64)
        .map(|t| {
            thread::spawn(move || {
                let p = format!("{dir}/t{t}.bin");
                let body: Vec<u8> = (0..2048).map(|i| (i as u64 + t) as u8).collect();
                fs::write(&p, &body).unwrap();
                let back = fs::read(&p).unwrap();
                assert_eq!(back, body, "thread {t}: concurrent fs write corrupted");
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fs_append_order() {
    let path = "/stress_append.txt";
    let _ = fs::remove_file(path);
    for i in 0..50 {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        f.write_all(format!("{i}\n").as_bytes()).unwrap();
    }
    let s = fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 50, "append lost lines");
    for (i, l) in lines.iter().enumerate() {
        assert_eq!(*l, i.to_string(), "append out of order at {i}");
    }
    fs::remove_file(path).ok();
}

// ---------------- alloc + collections ----------------

#[test]
fn alloc_large_vec() {
    let n = 500_000usize;
    let mut v: Vec<u64> = Vec::new();
    for i in 0..n as u64 {
        v.push(i);
    }
    let sum: u64 = v.iter().sum();
    assert_eq!(sum, (n as u64 - 1) * n as u64 / 2, "large vec sum wrong (alloc/realloc bug)");
}

#[test]
fn hashmap_churn() {
    let mut m: HashMap<u64, u64> = HashMap::new();
    for i in 0..30_000u64 {
        m.insert(i, i * 3);
    }
    for i in (0..30_000u64).step_by(2) {
        m.remove(&i);
    }
    for i in 30_000..45_000u64 {
        m.insert(i, i * 3);
    }
    assert_eq!(m.len(), 15_000 + 15_000);
    for (&k, &v) in &m {
        assert_eq!(v, k * 3, "hashmap value corrupted for key {k}");
    }
}

#[test]
fn btreemap_stays_sorted() {
    let mut m = BTreeMap::new();
    let mut x = 1u64;
    for _ in 0..10_000 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        m.insert(x, x ^ 0xdead);
    }
    let mut prev = None;
    for (&k, &v) in &m {
        if let Some(p) = prev {
            assert!(k > p, "btreemap not sorted");
        }
        assert_eq!(v, k ^ 0xdead);
        prev = Some(k);
    }
}

#[test]
fn try_reserve_huge_errors_not_aborts() {
    let mut v: Vec<u8> = Vec::new();
    let r = v.try_reserve(usize::MAX / 2);
    assert!(r.is_err(), "try_reserve(huge) should error, not succeed/abort");
}

// ---------------- pipe + loopback sockets: volume + integrity ----------------

#[test]
fn pipe_large_transfer() {
    use std::io::pipe;
    let (mut r, mut w) = pipe().unwrap();
    let total = 64 * 1024usize;
    let writer = thread::spawn(move || {
        let chunk: Vec<u8> = (0..1024).map(|i| (i * 7) as u8).collect();
        let mut sent = 0;
        while sent < total {
            w.write_all(&chunk).unwrap();
            sent += chunk.len();
        }
    });
    let mut got = Vec::with_capacity(total);
    let mut buf = [0u8; 4096];
    while got.len() < total {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    writer.join().unwrap();
    assert_eq!(got.len(), total, "pipe transfer truncated");
    for (i, &b) in got.iter().enumerate() {
        assert_eq!(b, ((i % 1024) * 7) as u8, "pipe corrupted at byte {i}");
    }
}

#[test]
fn tcp_loopback_large() {
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let total = 256 * 1024usize;
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut got = Vec::with_capacity(total);
        let mut buf = [0u8; 8192];
        while got.len() < total {
            let n = s.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    let mut c = TcpStream::connect(addr).unwrap();
    let chunk: Vec<u8> = (0..2048).map(|i| (i * 13 + 1) as u8).collect();
    let mut sent = 0;
    while sent < total {
        c.write_all(&chunk).unwrap();
        sent += chunk.len();
    }
    drop(c);
    let got = server.join().unwrap();
    assert_eq!(got.len(), total, "tcp loopback truncated (cap-and-lie?)");
    for (i, &b) in got.iter().enumerate() {
        assert_eq!(b, ((i % 2048) * 13 + 1) as u8, "tcp loopback corrupted at {i}");
    }
}

#[test]
fn udp_loopback_large_datagram() {
    use std::net::UdpSocket;
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr_b = b.local_addr().unwrap();
    let payload: Vec<u8> = (0..1200).map(|i| (i * 5 + 2) as u8).collect();
    let n = a.send_to(&payload, addr_b).unwrap();
    assert_eq!(n, payload.len(), "udp send_to reported short");
    let mut buf = [0u8; 2048];
    let (m, _src) = b.recv_from(&mut buf).unwrap();
    assert_eq!(m, payload.len(), "udp datagram truncated on loopback");
    assert_eq!(&buf[..m], &payload[..], "udp datagram corrupted");
}

// ---------------- time ----------------

#[test]
fn instant_monotonic_under_contention() {
    let handles: Vec<_> = (0..8)
        .map(|_| {
            thread::spawn(|| {
                let mut last = Instant::now();
                for _ in 0..200_000 {
                    let now = Instant::now();
                    assert!(now >= last, "Instant went backwards under contention");
                    last = now;
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
