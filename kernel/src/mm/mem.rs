//! User-facing memory capabilities: Memory budgets and Frame objects.
//!
//! A `Memory` object is a byte budget — the degenerate seL4 "untyped." Every
//! frame the kernel hands to userspace is debited against a Memory capability
//! the caller presented (ABI law L6: no allocation without an authorizing cap).
//! A `Frame` object names one physical frame so it can be mapped — and, because
//! handles transfer over IPC, shared — between address spaces.
use spin::Mutex;

const MEM_POOL: usize = 8;
const FRAME_POOL: usize = 16;

/// Memory a process is born holding (256 KiB). The kernel allocates user frames
/// ONLY through budgets, so the sum of outstanding budgets is bounded by RAM.
pub const BOOT_BUDGET: u64 = 64 * 4096;

#[derive(Clone, Copy)]
struct MemObj {
    in_use: bool,
    remaining: u64,
}

static MEMORY: Mutex<[MemObj; MEM_POOL]> = Mutex::new(
    [MemObj {
        in_use: false,
        remaining: 0,
    }; MEM_POOL],
);

#[derive(Clone, Copy)]
struct FrameObj {
    in_use: bool,
    phys: u64,
}

static FRAMES: Mutex<[FrameObj; FRAME_POOL]> = Mutex::new(
    [FrameObj {
        in_use: false,
        phys: 0,
    }; FRAME_POOL],
);

/// Grant a fresh Memory budget; returns its pool index (for `ObjectRef::Memory`),
/// or `None` if the pool is exhausted.
pub fn grant(budget: u64) -> Option<u8> {
    let mut m = MEMORY.lock();
    for i in 0..MEM_POOL {
        if !m[i].in_use {
            m[i] = MemObj {
                in_use: true,
                remaining: budget,
            };
            return Some(i as u8);
        }
    }
    None
}

/// Release a Memory budget slot (on process exit). The bytes are NOT refunded —
/// `pmm` is a bump allocator with no frame reclamation, so the frames really are
/// gone; only the pool slot is freed for reuse.
#[allow(dead_code)] // used by proc::kill (Phase 3)
pub fn release(idx: u8) {
    MEMORY.lock()[idx as usize].in_use = false;
}

/// Remaining budget on Memory `idx`.
pub fn remaining(idx: u8) -> u64 {
    MEMORY.lock()[idx as usize].remaining
}

/// Debit `bytes` from Memory `idx` if it can afford it; `true` on success.
#[allow(dead_code)] // used by sys_map in Phase 2
pub fn debit(idx: u8, bytes: u64) -> bool {
    let mut m = MEMORY.lock();
    let e = &mut m[idx as usize];
    if e.remaining >= bytes {
        e.remaining -= bytes;
        true
    } else {
        false
    }
}

/// Record a physical frame as a Frame object; returns its pool index.
#[allow(dead_code)] // used by sys_frame_alloc in Phase 4
pub fn frame_record(phys: u64) -> Option<u8> {
    let mut f = FRAMES.lock();
    for i in 0..FRAME_POOL {
        if !f[i].in_use {
            f[i] = FrameObj { in_use: true, phys };
            return Some(i as u8);
        }
    }
    None
}

/// Physical address behind Frame `idx`.
#[allow(dead_code)] // used by sys_frame_map in Phase 4
pub fn frame_phys(idx: u8) -> u64 {
    FRAMES.lock()[idx as usize].phys
}
