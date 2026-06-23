//! User-facing memory capabilities: Memory budgets and Frame objects.
//!
//! A `Memory` object is a byte budget — the degenerate seL4 "untyped." Every
//! frame the kernel hands to userspace is debited against a Memory capability
//! the caller presented (ABI law L6: no allocation without an authorizing cap).
//! A `Frame` object names one physical frame so it can be mapped — and, because
//! handles transfer over IPC, shared — between address spaces.
use crate::sync::DiagMutex;

// One Memory-object slot per live process (boot servers + each running spawn).
// Boot now starts 8 servers (…+ fb), so 8 left zero headroom for runtime spawns
// (they failed with NoMem). 24 gives ample room for servers + concurrent spawns.
const MEM_POOL: usize = 24;
const FRAME_POOL: usize = 32;

/// Memory a process is born holding (256 KiB). The kernel allocates user frames
/// ONLY through budgets, so the sum of outstanding budgets is bounded by RAM.
pub const BOOT_BUDGET: u64 = 64 * 4096;

#[derive(Clone, Copy)]
struct MemObj {
    in_use: bool,
    remaining: u64,
    /// Generation, bumped on release. A Memory HANDLE records the gen it was minted at
    /// (in its badge); a stale grant — a Memory cap that outlived its owner (it's
    /// grantable) while the slot was released and reused for another process's budget —
    /// is rejected, so it can't debit/map against the wrong budget. (Same reclaimable
    /// pattern as Frame/Reply. Today no flow grants a Memory cap cross-process, so this
    /// is defense for when one does — the cap's R_GRANT bit makes it a latent risk.)
    gen: u32,
}

static MEMORY: DiagMutex<[MemObj; MEM_POOL]> = DiagMutex::new("MEMORY",
    [MemObj {
        in_use: false,
        remaining: 0,
        gen: 0,
    }; MEM_POOL],
);

#[derive(Clone, Copy)]
struct FrameObj {
    in_use: bool,
    phys: u64,
    /// How many live address-space mappings reference this frame. The frame (and
    /// this pool slot) is reclaimed when the last mapping is torn down — so a
    /// shared zero-copy frame outlives any single mapper but doesn't leak.
    maps: u32,
    /// Generation: bumped every time this slot is freed. A Frame HANDLE records the
    /// generation it was minted at (in its badge); `frame_phys_checked` rejects a
    /// handle whose generation no longer matches. This closes the use-after-free where
    /// a grantable Frame handle outlives every mapping (the owner's teardown frees the
    /// physical frame) and is then mapped again to write a freed/reused frame.
    gen: u32,
}

static FRAMES: DiagMutex<[FrameObj; FRAME_POOL]> = DiagMutex::new("FRAMES",
    [FrameObj {
        in_use: false,
        phys: 0,
        maps: 0,
        gen: 0,
    }; FRAME_POOL],
);

/// Grant a fresh Memory budget; returns its pool index (for `ObjectRef::Memory`),
/// or `None` if the pool is exhausted.
pub fn grant(budget: u64) -> Option<u8> {
    let mut m = MEMORY.lock();
    for i in 0..MEM_POOL {
        if !m[i].in_use {
            let gen = m[i].gen; // preserved across reuse; only release bumps it
            m[i] = MemObj { in_use: true, remaining: budget, gen };
            return Some(i as u8);
        }
    }
    None
}

/// The current generation of Memory slot `idx` (stamped into a freshly-minted cap's
/// badge so a later stale-grant can be detected).
pub fn mem_gen(idx: u8) -> u32 {
    MEMORY.lock()[idx as usize].gen
}

/// True iff Memory slot `idx` is live AND at generation `gen` (the cap's badge value).
pub fn mem_gen_ok(idx: u8, gen: u32) -> bool {
    let m = MEMORY.lock();
    let e = &m[idx as usize];
    e.in_use && e.gen == gen
}

/// Release a Memory budget slot (on process exit) — frees the pool slot and bumps its
/// generation so any outstanding (granted-away) cap to it is detected as stale.
pub fn release(idx: u8) {
    let mut m = MEMORY.lock();
    m[idx as usize].in_use = false;
    m[idx as usize].gen = m[idx as usize].gen.wrapping_add(1);
}

/// Refund `bytes` to Memory budget `idx` (a spawner is credited the cost of a
/// child when that child dies and its frames are reclaimed).
pub fn credit(idx: u8, bytes: u64) {
    MEMORY.lock()[idx as usize].remaining += bytes;
}

/// Is `phys` a frame backing a live Frame object (zero-copy shared memory)? Such
/// frames are mapping-refcounted (see `frame_unmap`), not freed directly on
/// address-space teardown — a peer may still map them.
pub fn is_shared_frame(phys: u64) -> bool {
    FRAMES.lock().iter().any(|f| f.in_use && f.phys == phys)
}

/// Account an extra mapping of the shared Frame backing `phys` — fork: the child's
/// cloned AS re-maps the same shared frame, so its refcount must rise to balance the
/// extra `frame_unmap` at the child's teardown. No-op if `phys` isn't a tracked Frame.
pub fn frame_inc_map_by_phys(phys: u64) {
    let mut f = FRAMES.lock();
    if let Some(e) = f.iter_mut().find(|e| e.in_use && e.phys == phys) {
        e.maps += 1;
    }
}

/// Drop one mapping of the Frame backing `phys` (called as an address space is
/// torn down). When the last mapping goes, free the physical frame and the pool
/// slot — so shared frames are reclaimed exactly when nobody maps them anymore.
pub fn frame_unmap(phys: u64) {
    let freed = {
        let mut f = FRAMES.lock();
        match f.iter_mut().find(|e| e.in_use && e.phys == phys) {
            Some(e) => {
                e.maps = e.maps.saturating_sub(1);
                if e.maps == 0 {
                    e.in_use = false;
                    e.phys = 0; // don't leave a usable phys behind a freed slot
                    e.gen = e.gen.wrapping_add(1); // invalidate any outstanding handles
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    };
    if freed {
        super::pmm::free_frame(phys);
    }
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

/// Record a physical frame as a Frame object; returns `(pool index, generation)`.
/// The generation is preserved across reuse of the slot (it's only bumped on free),
/// so a handle minted at an older generation can be detected as stale.
#[allow(dead_code)] // used by sys_frame_alloc in Phase 4
pub fn frame_record(phys: u64) -> Option<(u8, u32)> {
    let mut f = FRAMES.lock();
    for i in 0..FRAME_POOL {
        if !f[i].in_use {
            let gen = f[i].gen;
            f[i] = FrameObj { in_use: true, phys, maps: 0, gen };
            return Some((i as u8, gen));
        }
    }
    None
}

/// Atomically CHECK that Frame `idx` is live at generation `gen` AND account a new
/// mapping (`maps += 1`), returning its phys — all under one FRAMES lock. Doing the
/// gen-check and the map-count bump together means a concurrent address-space teardown
/// (`frame_unmap`, also under FRAMES) can't drive `maps` to 0 and free the physical
/// frame in the window between the check and the actual `map_to` (the use-after-free the
/// bare check-then-later-inc left open). Returns `None` (caller maps nothing) for a
/// freed/reused slot or a generation mismatch — the UAF guard for `sys_frame_map`.
pub fn frame_checkout(idx: u8, gen: u32) -> Option<u64> {
    let mut f = FRAMES.lock();
    let e = &mut f[idx as usize];
    if e.in_use && e.gen == gen {
        e.maps += 1;
        Some(e.phys)
    } else {
        None
    }
}
