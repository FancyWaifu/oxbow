//! Spawnable program images.
//!
//! With no filesystem yet, programs come from Limine modules loaded at boot.
//! Modules the kernel does NOT boot-spawn (pong, beta, hello) are instead
//! *registered* here as named blobs and exposed to userspace as `Image` handle
//! capabilities. `sys_spawn` takes such a handle, loads the ELF into a fresh
//! address space, and starts it — so a process can only launch images it was
//! granted (zero ambient authority; spawn-by-handle, not spawn-by-name-string).
//!
//! Module bytes live in BOOTLOADER_RECLAIMABLE memory, which `pmm` never
//! consumes (it only takes USABLE regions), so these pointers stay valid for the
//! life of the system.
use spin::Mutex;

/// Max distinct spawnable images. Bump alongside the boot module list.
pub const MAX_IMAGES: usize = 12;

#[derive(Clone, Copy)]
#[allow(dead_code)] // name/ptr/len read by the sys_spawn path (next phase)
struct ImageEntry {
    in_use: bool,
    name: [u8; 16],
    name_len: usize,
    ptr: *const u8,
    len: usize,
}

// SAFETY: the pointer names bootloader-reclaimable module memory the kernel
// never frees or hands to the frame allocator; it is read-only after boot.
unsafe impl Send for ImageEntry {}

static IMAGES: Mutex<[ImageEntry; MAX_IMAGES]> = Mutex::new(
    [ImageEntry {
        in_use: false,
        name: [0; 16],
        name_len: 0,
        ptr: core::ptr::null(),
        len: 0,
    }; MAX_IMAGES],
);

/// Register a module's bytes under `name`; returns its registry index.
pub fn register(name: &[u8], bytes: &'static [u8]) -> u8 {
    let mut imgs = IMAGES.lock();
    for i in 0..MAX_IMAGES {
        if !imgs[i].in_use {
            let n = core::cmp::min(name.len(), 16);
            let mut nb = [0u8; 16];
            nb[..n].copy_from_slice(&name[..n]);
            imgs[i] = ImageEntry {
                in_use: true,
                name: nb,
                name_len: n,
                ptr: bytes.as_ptr(),
                len: bytes.len(),
            };
            return i as u8;
        }
    }
    panic!("image: registry full");
}

/// Registry index of the image registered under `name`, or `None`.
pub fn find(name: &[u8]) -> Option<u8> {
    let imgs = IMAGES.lock();
    for i in 0..MAX_IMAGES {
        if imgs[i].in_use && imgs[i].name[..imgs[i].name_len] == *name {
            return Some(i as u8);
        }
    }
    None
}

/// The ELF bytes for registered image `idx`, or `None` if the slot is empty.
#[allow(dead_code)] // used by sys_spawn (next phase)
pub fn bytes(idx: u8) -> Option<&'static [u8]> {
    let imgs = IMAGES.lock();
    let e = imgs.get(idx as usize)?;
    if !e.in_use {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(e.ptr, e.len) })
}

/// The registered name of image `idx` (for boot logging), or `b""`.
#[allow(dead_code)] // used by sys_spawn (next phase)
pub fn name(idx: u8) -> [u8; 16] {
    IMAGES.lock()[idx as usize].name
}

/// Length of the registered name of image `idx`.
#[allow(dead_code)] // used by sys_spawn (next phase)
pub fn name_len(idx: u8) -> usize {
    IMAGES.lock()[idx as usize].name_len
}
