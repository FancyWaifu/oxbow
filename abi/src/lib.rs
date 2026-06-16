//! oxbow-abi — the single source of truth for the oxbow kernel ABI.
//!
//! Syscall numbers, rights bits, error codes, and the IPC message layout. Both
//! the kernel and userland depend on this crate, so neither can drift from the
//! other. This is the machine-readable form of `docs/abi-v0.md` — keep them in
//! lockstep; the spec is normative.
#![no_std]

/// ABI revision. Bumped on any breaking change to the items below.
pub const ABI_VERSION: u32 = 0;

// ---------------------------------------------------------------------------
// Handles (§3)
// ---------------------------------------------------------------------------

/// A handle is an opaque index into the calling process's private handle table.
/// Its integer value is meaningless in any other process (law L2).
pub type Handle = u32;

/// The reserved invalid handle. Index 0 is permanently unoccupied; valid
/// handles are `1..HANDLE_TABLE_SIZE`.
pub const HANDLE_NULL: Handle = 0;

/// Flat per-process handle table size in v0. Explicitly not a CNode tree.
pub const HANDLE_TABLE_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// Rights bitflags (§3.2) — bits 0..16 generic, 16..32 object-specific
// ---------------------------------------------------------------------------

/// May send / call on an Endpoint.
pub const R_SEND: u32 = 1 << 0;
/// May recv on an Endpoint.
pub const R_RECV: u32 = 1 << 1;
/// Handle may be transferred in a message.
pub const R_GRANT: u32 = 1 << 2;
/// Handle may be the source of `sys_attenuate`.
pub const R_ATTENUATE: u32 = 1 << 3;
/// Console-specific: may write bytes to the console.
/// Frame-specific (reused bit): the frame may be mapped WRITABLE.
pub const R_WRITE: u32 = 1 << 16;
/// Memory: may debit (sys_map / sys_frame_alloc). Frame: may be mapped.
pub const R_MAP: u32 = 1 << 17;
/// IoPort: may read a port.
pub const R_IN: u32 = 1 << 18;
/// IoPort: may write a port.
pub const R_OUT: u32 = 1 << 19;
/// IrqLine: may bind the line to a notification.
pub const R_BIND: u32 = 1 << 20;
/// IrqLine: may ack (re-arm) the line.
pub const R_ACK: u32 = 1 << 21;
/// Image: may be spawned into a new process (`sys_spawn`).
pub const R_SPAWN: u32 = 1 << 22;

// Notifications reuse the IPC verbs: signalling is "send", waiting is "recv".
pub const R_SIGNAL: u32 = R_SEND;
pub const R_WAIT: u32 = R_RECV;

// Mapping protection flags for sys_map / sys_frame_map / sys_protect (NOT rights;
// per call). W^X (law L4) still holds: the kernel rejects WRITE|EXEC together —
// but `sys_protect` allows the RW->RX *transition* a JIT needs (e.g. tcc -run).
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2; // implies read
pub const PROT_EXEC: u64 = 4; // read + execute (never combined with WRITE)

// ---------------------------------------------------------------------------
// Syscall numbers (§4.3) — passed in rax
// ---------------------------------------------------------------------------

pub const SYS_SEND: u64 = 0;
pub const SYS_RECV: u64 = 1;
pub const SYS_CALL: u64 = 2;
pub const SYS_REPLY: u64 = 3;
pub const SYS_ATTENUATE: u64 = 4;
pub const SYS_CLOSE: u64 = 5;
pub const SYS_CONSOLE_WRITE: u64 = 6;
pub const SYS_EXIT: u64 = 7;

// v1 additions — user-driven memory (numbers 8+ were reserved by §4.2).
pub const SYS_MAP: u64 = 8; // sys_map(mem, vaddr, len, prot)
pub const SYS_FRAME_ALLOC: u64 = 9; // sys_frame_alloc(mem) -> Frame handle
pub const SYS_FRAME_MAP: u64 = 10; // sys_frame_map(frame, vaddr, prot)

// v1 additions — IRQ / device drivers.
pub const SYS_NOTIF_CREATE: u64 = 11; // () -> Notification handle
pub const SYS_NOTIF_SIGNAL: u64 = 12; // (notif)          needs R_SIGNAL
pub const SYS_NOTIF_WAIT: u64 = 13; // (notif) -> count   needs R_WAIT
pub const SYS_IO_IN: u64 = 14; // (ioport, port) -> byte  needs R_IN
pub const SYS_IO_OUT: u64 = 15; // (ioport, port, value)  needs R_OUT
pub const SYS_IRQ_BIND: u64 = 16; // (irq, notif)          needs R_BIND + R_SIGNAL
pub const SYS_IRQ_ACK: u64 = 17; // (irq)                  needs R_ACK

// Process spawning (§13).
pub const SYS_SPAWN: u64 = 18; // (image, mem, &MsgBuf, exit_notif) -> pid
pub const SYS_EP_CREATE: u64 = 19; // () -> fresh Endpoint handle

// Badged endpoints (§14).
pub const SYS_MINT: u64 = 20; // (ep, badge, new_rights) -> badged Endpoint handle

// PCI / MMIO (§18). A PciDevice capability scopes access to ONE device.
pub const SYS_PCI_READ: u64 = 21; // (pcidev, offset) -> u32   needs R_IN
pub const SYS_PCI_WRITE: u64 = 22; // (pcidev, offset, value)  needs R_OUT
pub const SYS_PCI_BAR_MAP: u64 = 23; // (pcidev, bar, vaddr)   needs R_MAP; maps BAR MMIO

// DMA memory (§19). Allocate one frame, map it writable+cacheable into the
// caller's AS at `vaddr`, and RETURN its physical address (in rdx) — a driver
// needs known physical addresses to program a device's ring-base registers and
// descriptor buffer pointers. Paid from the caller's Memory budget (R_MAP).
pub const SYS_DMA_ALLOC: u64 = 24; // (mem, vaddr) -> phys      needs R_MAP

/// Monotonic uptime in milliseconds (in rdx). Ambient/unprivileged — a clock is
/// not a capability — needed by timer-driven userland (e.g. smoltcp's TCP).
pub const SYS_UPTIME_MS: u64 = 25; // () -> u64 ms

/// Change the protection of already-mapped user pages (the JIT/exec primitive).
/// W^X-enforced: `prot` may be PROT_READ|PROT_WRITE or PROT_READ|PROT_EXEC, never
/// both — so a JIT writes code into an RW page then flips it to RX. Needs R_MAP.
pub const SYS_PROTECT: u64 = 26; // (mem, vaddr, len, prot)

/// exec-from-fs (§33): spawn a fresh process from an ELF image the caller
/// supplies as bytes (e.g. read from a filesystem file), rather than from a
/// boot-granted Image capability. Same MsgBuf grant/budget protocol as
/// `sys_spawn`. The authority is "run what you can read and afford": the caller
/// proved read access to the bytes (via a file cap) and pays with a Memory cap.
pub const SYS_SPAWN_BYTES: u64 = 27; // (buf, len, mem, &MsgBuf, exit_notif) -> pid

/// getentropy (§36) — fill a user buffer (<=256 bytes) with CSPRNG bytes. A
/// handle-free syscall, like `sys_exit`: it conveys NO authority over any object
/// (you cannot reach a kernel object or another process through random bytes), so
/// it does not violate L1's "operate on a handle you hold" rule. Backs the libc
/// arc4random and the stack-protector cookie.
pub const SYS_GETENTROPY: u64 = 28; // (buf, len) -> 0 / E_MSG (len>256) / E_FAULT

/// pledge (§37) — a process voluntarily restricts itself to a subset of syscall
/// CLASSES (the OpenBSD pledge(2) model, adapted to oxbow's verbs). One-way: the
/// new promise set is intersected with the current one, so authority can only be
/// dropped, never regained. After pledging, calling a syscall outside the
/// permitted classes is FAIL-CLOSED — the kernel kills the process immediately
/// (no error return; an exploit that hijacks control flow trips the pledge and
/// dies before it can do damage). Always-permitted regardless of pledge: exit,
/// pledge itself, and close (releasing resources). Defense-in-depth ON TOP of
/// capabilities: even with a handle, you cannot use a class you pledged away.
pub const SYS_PLEDGE: u64 = 29; // (promises) -> 0

/// Pledge promise classes (bitmask). Unpledged = all bits (u64::MAX).
pub const PLEDGE_STDIO: u64 = 1 << 0; // console_write, getentropy, uptime
pub const PLEDGE_IPC: u64 = 1 << 1; // send, recv, call, reply, ep_create, mint
pub const PLEDGE_MEM: u64 = 1 << 2; // map, protect, frame_alloc/map, dma_alloc
pub const PLEDGE_SPAWN: u64 = 1 << 3; // spawn, spawn_bytes
pub const PLEDGE_CAP: u64 = 1 << 4; // attenuate
pub const PLEDGE_IO: u64 = 1 << 5; // io_in/out, pci_*, irq_*
pub const PLEDGE_NOTIF: u64 = 1 << 6; // notif_create/signal/wait

/// immutable (§38) — OpenBSD mimmutable(2): permanently lock the protection of a
/// mapped range. After this, sys_map or sys_protect touching any page in the
/// range is refused (E_RIGHTS) — even a W^X-LEGAL flip like RW->RX. A runtime
/// maps its code, sets it RX, marks it immutable, and now nothing (not even the
/// process itself, post-exploit) can make it writable or remap it. Hardens W^X
/// (L4) from "never W and X at once" to "this text can never change again".
/// Gated on the Memory cap (R_MAP), like map/protect. One-way: no un-immutable.
pub const SYS_IMMUTABLE: u64 = 30; // (mem, vaddr, len) -> 0 / E_NOMEM (table full)

/// Pipes (§39) — a kernel-buffered unidirectional byte channel, the primitive
/// behind shell pipelines (`cmd1 | cmd2`). `sys_pipe` mints one capability with
/// full rights (R_IN|R_OUT); the holder attenuates it to a write end (R_OUT, for
/// cmd1's stdout) and a read end (R_IN, for cmd2's stdin) and grants each to a
/// child. A read blocks while the buffer is empty and returns 0 (EOF) once the
/// write side is closed; a write blocks while the buffer is full. EOF is signaled
/// explicitly by the pipeline owner via `sys_pipe_eof` (e.g. after cmd1 exits).
pub const SYS_PIPE: u64 = 31; // () -> pipe handle (R_IN|R_OUT|R_GRANT|R_ATTENUATE)
pub const SYS_PIPE_READ: u64 = 32; // (pipe, buf, len) -> count (0 = EOF). Needs R_IN
pub const SYS_PIPE_WRITE: u64 = 33; // (pipe, buf, len) -> count. Needs R_OUT
pub const SYS_PIPE_EOF: u64 = 34; // (pipe) -> 0. Mark the write side closed (R_OUT)
pub const SYS_FB_INFO: u64 = 35; // (fb) -> packed geometry (R_MAP). See fb server.
pub const SYS_FB_MAP: u64 = 36; // (fb, vaddr) -> 0. Map the framebuffer RW (R_MAP)
// Bidirectional byte+capability channel (§40) — the socketpair/SCM_RIGHTS prim.
pub const SYS_CHANNEL_PAIR: u64 = 37; // () -> rdx = h0 | h1<<32 (two channel ends)
pub const SYS_CHANNEL_SEND: u64 = 38; // (h, buf, len, caps_ptr, ncaps) -> rdx = nbytes
pub const SYS_CHANNEL_RECV: u64 = 39; // (h, buf, len, caps_out, ncaps_max|flags<<32)
pub const SYS_CHANNEL_CLOSE: u64 = 40; // (h) -> 0. Close this end; peer sees EOF.
pub const SYS_CHANNEL_POLL: u64 = 41; // (h) -> rdx readiness: 1=readable 2=eof 4=writable
// Shared multi-page memory (§41) — backs memfd/mmap + Wayland wl_shm buffers.
pub const SYS_SHM_CREATE: u64 = 42; // (mem, pages) -> Shm handle (R_MAP|R_WRITE|R_GRANT)
pub const SYS_SHM_MAP: u64 = 43; // (shm, vaddr) -> size. Map all pages RW at vaddr.
pub const SYS_CAP_TYPE: u64 = 44; // (h) -> rdx cap kind (CAP_* below). For fd-passing.
pub const SYS_CAP_DUP: u64 = 45; // (h) -> a fresh handle to the SAME object (same rights)
/// Capability kinds reported by SYS_CAP_TYPE (so recvmsg can reconstruct the
/// right fd flavor from a passed handle). 0 = anything else.
pub const CAP_OTHER: u64 = 0;
pub const CAP_CHANNEL: u64 = 1;
pub const CAP_SHM: u64 = 2;
/// SYS_CHANNEL_RECV flag (in the high 32 bits of a5): don't block on empty.
pub const CHAN_NONBLOCK: u64 = 1;

// ---------------------------------------------------------------------------
// Error codes (§6) — returned in rax; values are stable forever (append-only)
// ---------------------------------------------------------------------------

/// Syscall error codes. `0` is success and is represented by `Ok` in
/// [`SysResult`], so it is intentionally absent from this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum SysError {
    /// Index out of range or slot empty.
    BadHandle = 1,
    /// Object is not the type this syscall expects.
    BadType = 2,
    /// Handle lacks a required right; or attenuation rights not a subset.
    Rights = 3,
    /// Bad user pointer (unmapped / not user-accessible / misaligned).
    Fault = 4,
    /// Message exceeds `MSG_*` limits, or a length is too large.
    Msg = 5,
    /// A handle table is full.
    NoSlots = 6,
    /// Kernel object pool exhausted (law L6).
    NoMem = 7,
    /// Peer or object destroyed while blocked / reply abandoned.
    Gone = 8,
    /// Reserved: non-blocking variants are v1; never returned in v0.
    WouldBlock = 9,
    /// Unknown syscall number.
    Nosys = 10,
}

/// The result of a syscall: `Ok(())`, or `Ok(handle)` for syscalls that return a
/// freshly allocated handle in rdx, or `Err(SysError)` from rax.
pub type SysResult<T = ()> = Result<T, SysError>;

impl SysError {
    /// Decode a raw rax value: `0` is success, anything else an error code.
    /// Unknown nonzero values are mapped to [`SysError::Nosys`] defensively.
    pub fn from_raw(rax: u64) -> SysResult {
        match rax {
            0 => Ok(()),
            1 => Err(SysError::BadHandle),
            2 => Err(SysError::BadType),
            3 => Err(SysError::Rights),
            4 => Err(SysError::Fault),
            5 => Err(SysError::Msg),
            6 => Err(SysError::NoSlots),
            7 => Err(SysError::NoMem),
            8 => Err(SysError::Gone),
            9 => Err(SysError::WouldBlock),
            _ => Err(SysError::Nosys),
        }
    }
}

// ---------------------------------------------------------------------------
// IPC message format (§5)
// ---------------------------------------------------------------------------

/// Inline payload words (64 bytes).
pub const MSG_DATA_WORDS: usize = 8;
/// Transferable handle slots per message.
pub const MSG_HANDLES: usize = 4;

/// Fixed-size IPC message. The kernel copies this between sender and receiver at
/// rendezvous (law L7 — no kernel-side buffering). 104 bytes, 8-byte aligned.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct MsgBuf {
    /// User-defined label; the kernel never interprets it.
    pub tag: u64,
    /// Valid words in `data`, `0..=MSG_DATA_WORDS`.
    pub data_len: u32,
    /// Valid slots in `handles`, `0..=MSG_HANDLES`.
    pub handle_count: u32,
    /// Inline payload.
    pub data: [u64; MSG_DATA_WORDS],
    /// Sender: handles to transfer (each needs `R_GRANT`).
    /// Receiver: kernel-written fresh indices.
    pub handles: [Handle; MSG_HANDLES],
    /// Receiver: the badge of the capability the sender invoked, kernel-written
    /// on every delivery (0 = unbadged; always 0 on a reply). Sender: ignored —
    /// the kernel overwrites it with the invoked cap's badge, so it is
    /// unforgeable. See §14 (badged endpoints).
    pub badge: u64,
}

// MsgBuf is the cross-ABI wire struct; its size feeds `check_user`. Keep it
// pinned so a layout drift can never silently desync kernel and userland.
const _: () = assert!(core::mem::size_of::<MsgBuf>() == 104);

impl MsgBuf {
    /// An empty message with the given tag and no payload or handles.
    pub const fn new(tag: u64) -> Self {
        MsgBuf {
            tag,
            data_len: 0,
            handle_count: 0,
            data: [0; MSG_DATA_WORDS],
            handles: [HANDLE_NULL; MSG_HANDLES],
            badge: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Well-known boot handles & protocol tags (§7)
// ---------------------------------------------------------------------------

/// Endpoint handle the kernel grants the first server (R_SEND | R_ATTENUATE).
pub const BOOT_EP: Handle = 1;
/// Console handle the kernel grants the first server (R_WRITE | R_ATTENUATE).
pub const BOOT_CONSOLE: Handle = 2;
/// Memory budget handle a process is born holding (R_MAP | R_GRANT | R_ATTENUATE).
pub const BOOT_MEM: Handle = 3;
/// Tick notification (module 0 only): the timer signals it ~1 Hz. R_WAIT.
pub const BOOT_TICK: Handle = 4;
/// Driver (module 2) boot handles: the keyboard IRQ line and i8042 I/O ports.
pub const BOOT_IRQ: Handle = 4; // IrqLine(1) — R_BIND|R_ACK
pub const BOOT_KBD_DATA: Handle = 5; // IoPort{0x60,1}
pub const BOOT_KBD_STATUS: Handle = 6; // IoPort{0x64,1}
/// The TTY endpoint: kbd/shell hold R_SEND, tty holds R_RECV (slot 7).
pub const BOOT_TTY: Handle = 7;
/// Serial driver (module 5) boot handles: the COM1 IRQ line + 16550 RX ports.
/// The driver only ever READS, so the port caps are granted R_IN with no R_OUT;
/// the kernel keeps exclusive ownership of every UART config/TX register.
pub const BOOT_SERIAL_IRQ: Handle = 4; // IrqLine(4) — R_BIND|R_ACK
pub const BOOT_SERIAL_RBR: Handle = 5; // IoPort{0x3F8,1} — R_IN (RBR, read side)
pub const BOOT_SERIAL_LSR: Handle = 6; // IoPort{0x3FD,1} — R_IN (line status)

// --- Process spawning (§13) ------------------------------------------------
/// Image capabilities the shell is born holding (R_SPAWN | R_GRANT | R_ATTENUATE).
pub const BOOT_IMG_HELLO: Handle = 8;
pub const BOOT_IMG_PONG: Handle = 9;
pub const BOOT_IMG_BETA: Handle = 10;
pub const BOOT_IMG_BADGE: Handle = 11; // §14 badged-endpoint demo server
pub const BOOT_IMG_CAT: Handle = 13; // spawned coreutil: cat (gets a file cap)
pub const BOOT_IMG_LS: Handle = 14; // spawned coreutil: ls (gets a dir cap)
pub const BOOT_IMG_MKDIR: Handle = 15; // spawned coreutil: mkdir (dir cap + argv)
pub const BOOT_IMG_TOUCH: Handle = 16; // spawned coreutil: touch (dir cap + argv)
pub const BOOT_IMG_RM: Handle = 17; // spawned coreutil: rm (dir cap + argv)
pub const BOOT_IMG_MV: Handle = 18; // spawned coreutil: mv (dir cap + argv)
pub const BOOT_IMG_CP: Handle = 19; // spawned coreutil: cp (dir cap + argv)

// --- Networking (§18) ------------------------------------------------------
/// The net driver's PciDevice capability (the NIC), at this boot handle.
pub const BOOT_PCI: Handle = 8;
/// Fixed vaddr where the net driver maps the NIC's MMIO BAR0.
pub const NET_MMIO: u64 = 0x4000_0000;
/// The net driver's IrqLine capability for the NIC's interrupt (R_BIND|R_ACK).
pub const BOOT_NET_IRQ: Handle = 9;
/// Fixed vaddr base where the net driver maps its first DMA page (rings +
/// packet buffers map at NET_DMA + n*0x1000).
pub const NET_DMA: u64 = 0x4010_0000;

/// Block driver (virtio-blk, modern/MMIO): the device's PciDevice cap is granted
/// at `BOOT_PCI` in the blk server. It maps the device BAR (named by the virtio
/// PCI capability structure) at `BLK_MMIO` and its DMA pages (virtqueue + request
/// buffers) at `BLK_DMA + n*0x1000`.
pub const BLK_MMIO: u64 = 0x4020_0000;
pub const BLK_DMA: u64 = 0x4030_0000;

// --- Block service IPC (§24) ----------------------------------------------
/// The block driver also serves a SECTOR read/write endpoint (the root of block
/// authority, EP4). It owns this UNBADGED at `BOOT_EP` with R_RECV; the fs server
/// holds a SEND cap to it at `BOOT_BLK_EP` and uses it to persist its writable
/// files to disk. Sectors are 512 bytes; the service keeps a one-sector
/// write-back cache, so reads/writes are byte-granular streams that the fs server
/// drives sequentially. (No badge: there is a single disk and a single client.)
pub const BOOT_BLK_EP: Handle = 28;
/// The largest payload a block read/write message carries (bytes). Bounded by the
/// 64-byte `data` array: WRITE puts sector+offset+count in data[0..3] (24 bytes)
/// then the payload, so 24 + BLK_CHUNK must be <= 64 — hence 40, not 48. (At 48
/// the last 8 bytes of every write chunk fell outside `data` and were dropped,
/// silently corrupting every block written.)
pub const BLK_CHUNK: usize = 40;
/// READ: data[0]=sector (LBA), data[1]=offset within sector (0..512). Reply:
/// data[0]=count (bytes, 0 on error/EOF), payload bytes from offset 8.
pub const TAG_BLK_READ: u64 = u32::from_le_bytes(*b"BKRD") as u64;
/// WRITE: data[0]=sector, data[1]=offset, data[2]=count (<=BLK_CHUNK), payload
/// bytes from offset 24. The service buffers into its cached sector (write-back).
/// Reply: data[0]=count written (0 on error).
pub const TAG_BLK_WRITE: u64 = u32::from_le_bytes(*b"BKWR") as u64;
/// FLUSH: commit the cached dirty sector to disk. Reply: data[0]=status (0 ok).
pub const TAG_BLK_FLUSH: u64 = u32::from_le_bytes(*b"BKFL") as u64;

// --- Bulk (shared-memory) block transfer -----------------------------------
// The byte-stream READ/WRITE above moves <=40 bytes per IPC — ~13 round-trips
// per 512-byte sector, far too slow for a filesystem. A client instead shares a
// page-sized Frame with the block service: blk memcpys whole sectors between
// that shared page and its DMA buffer, so a multi-sector transfer is ONE IPC.
/// Sectors per shared-frame transfer (a 4 KiB frame = 8 x 512-byte sectors).
pub const BLK_XFER_SECTORS: u64 = 8;
/// Where the block service maps the client's shared transfer frame.
pub const BLK_SHARED: u64 = 0x4040_0000;
/// ATTACH: handles[0] = a writable Frame cap; the block service maps it as the
/// shared transfer buffer. Reply: data[0]=status (0 ok).
pub const TAG_BLK_ATTACH: u64 = u32::from_le_bytes(*b"BKAT") as u64;
/// READN: read data[1] sectors starting at LBA data[0] from disk INTO the shared
/// frame (sector i at frame offset i*512). Reply: data[0]=status.
pub const TAG_BLK_READN: u64 = u32::from_le_bytes(*b"BKRN") as u64;
/// WRITEN: write data[1] sectors starting at LBA data[0] FROM the shared frame to
/// disk. Reply: data[0]=status.
pub const TAG_BLK_WRITEN: u64 = u32::from_le_bytes(*b"BKWN") as u64;

// --- Filesystem (§15) ------------------------------------------------------
/// The shell's root-directory capability: a BADGED endpoint to the fs server,
/// badge = FS_ROOT. Open files relative to it (directories are capabilities).
pub const BOOT_FS_ROOT: Handle = 12;
/// The root directory's node id (the badge the kernel stamps on BOOT_FS_ROOT).
pub const FS_ROOT: u64 = 1;
/// Fixed vaddr where the kernel maps the tar initrd (read-only) into the fs
/// server's address space at boot; the fs parses USTAR from here.
pub const FS_INITRD: u64 = 0x1000_0000;
/// Node kinds, reported by OPEN/READDIR.
pub const FS_DIR: u64 = 1;
pub const FS_FILE: u64 = 2;
/// FS request tags (sent through a dir/file capability; the badge = the node).
/// OPEN(dir): `data` = the name bytes (NUL-terminated). Reply: `data[0]` = status
/// (0 ok / 1 not-found), `data[1]` = kind, `data[2]` = size, `handles[0]` = a
/// freshly-minted badged capability to the resolved node.
pub const TAG_FS_OPEN: u64 = u32::from_le_bytes(*b"FSOP") as u64;
/// READ(file): `data[0]` = byte offset. Reply: `data[0]` = count (0 = EOF),
/// `data[1..]` = up to 56 bytes of content.
pub const TAG_FS_READ: u64 = u32::from_le_bytes(*b"FSRD") as u64;
/// READDIR(dir): `data[0]` = cursor index. Reply: `data[0]` = 1 if an entry is
/// present (else 0 = end), `data[1]` = kind, `data[2..]` = the entry name.
pub const TAG_FS_READDIR: u64 = u32::from_le_bytes(*b"FSDR") as u64;
/// CREATE(dir): `data` = name. Create-or-truncate a file. Reply: `data[0]` =
/// status (0 ok / 1 fail), `handles[0]` = a badged capability to the file.
pub const TAG_FS_CREATE: u64 = u32::from_le_bytes(*b"FSCR") as u64;
/// WRITE(file): `data[0]` = offset, `data[1]` = count, `data[2..]` = up to 48
/// bytes. Reply: `data[0]` = count actually written (0 = no space).
pub const TAG_FS_WRITE: u64 = u32::from_le_bytes(*b"FSWR") as u64;
/// MKDIR(dir): `data` = name. Reply: `data[0]` = status (0 ok / 1 fail).
pub const TAG_FS_MKDIR: u64 = u32::from_le_bytes(*b"FSMD") as u64;
/// UNLINK(dir): `data` = name. Removes a file or empty directory. Reply:
/// `data[0]` = status (0 ok / 1 not-found / 2 directory-not-empty).
pub const TAG_FS_UNLINK: u64 = u32::from_le_bytes(*b"FSRM") as u64;
/// RENAME(dir): `data` = old name NUL then new name NUL. Renames a child within
/// the directory. Reply: `data[0]` = status (0 ok / 1 fail).
pub const TAG_FS_RENAME: u64 = u32::from_le_bytes(*b"FSMV") as u64;
/// SYNC(root): persist every writable file + directory to the block device, so
/// the tree survives a reboot. Sent on the root dir cap. Reply: data[0]=status
/// (0 ok), data[1]=entries written. The fs auto-restores from disk at boot.
pub const TAG_FS_SYNC: u64 = u32::from_le_bytes(*b"FSSY") as u64;

// --- Socket capability API (§21) -------------------------------------------
/// A client's control capability to the net server: a BADGED endpoint with the
/// NET_CTL badge. `udp_bind` on it mints a fresh badged UDP-socket capability.
pub const BOOT_NET_EP: Handle = 20;
/// Spawnable image: the DRIFT client (SSE crypto; needs the kernel's FPU support).
pub const BOOT_IMG_DRIFT: Handle = 21;
/// Spawnable image: a C program (clang-compiled) over the oxbow libc shim.
pub const BOOT_IMG_CCHELLO: Handle = 22;
/// Spawnable image: TinyCC (the C compiler) running on oxbow.
pub const BOOT_IMG_TCC: Handle = 23;
pub const BOOT_IMG_LUA: Handle = 24; // the Lua 5.4 interpreter
pub const BOOT_IMG_UPY: Handle = 25; // the MicroPython interpreter
pub const BOOT_IMG_QJS: Handle = 26; // the QuickJS JavaScript engine
pub const BOOT_IMG_CURL: Handle = 27; // curl (HTTP, no TLS)
pub const BOOT_IMG_JAIL: Handle = 29; // jail — the capability-confinement showcase
pub const BOOT_IMG_FSTEST: Handle = 30; // fstest — lwext4/ext2 port self-test
pub const BOOT_IMG_CARES: Handle = 31; // cares-test — c-ares DNS resolver port
pub const BOOT_IMG_FFI: Handle = 33; // ffi-test — libffi (x86_64 SysV) port
pub const BOOT_IMG_WL: Handle = 34; // wl-test — libwayland wire-core port
pub const BOOT_IMG_WLCLIENT: Handle = 35; // wlclient — the compositor's Wayland client
pub const BOOT_IMG_XKB: Handle = 37; // xkb-test — libxkbcommon keymap/keysym port (36 = BOOT_INPUT_CHAN)
pub const BOOT_IMG_VTERM: Handle = 38; // vterm-test — libvterm terminal state machine port
pub const BOOT_IMG_FT: Handle = 39; // ft-test — FreeType glyph rasterizer port
pub const BOOT_IMG_OXTERM: Handle = 40; // oxterm — the Wayland terminal client

/// The framebuffer capability, granted to the `fb` server at boot. Gates
/// SYS_FB_INFO (geometry) + SYS_FB_MAP (map the pixels RW). §34 (graphics).
pub const BOOT_FB: Handle = 32;
/// A kernel-created channel for keyboard events: the `kbd` driver holds the send
/// end and `oxcomp` the receive end (both at this handle). kbd writes each key
/// byte; the compositor's event loop watches the fd and turns bytes into
/// wl_keyboard events (§47, on-screen input). Distinct from BOOT_TTY so the
/// serial console keeps working alongside the graphical path.
pub const BOOT_INPUT_CHAN: Handle = 36;
/// A kernel-created channel that mirrors the tty's console output to the
/// graphical terminal (oxterm): the `tty` holds the send end, the compositor the
/// receive end which it passes to oxterm at spawn — so the shell/login text the
/// tty prints also renders on screen (§53). Handle 41 (37–40 are images).
pub const BOOT_TERM_CHAN: Handle = 41;
/// Fixed vaddr where the fb server maps the linear framebuffer.
pub const FB_MMIO: u64 = 0x5000_0000;
/// The control-channel badge (distinct from any socket id, which are 1..=N).
pub const NET_CTL: u64 = 0x00C0_FFEE;
/// Bind a UDP socket: request on the NET_CTL cap, data[0]=port (0=ephemeral).
/// Reply: data[0]=status, data[1]=bound port, handles[0]=badged socket cap.
/// Query the network config on the NET_CTL cap: reply data[0..4] = the leased
/// DNS resolver IP (so clients resolve via the DHCP-given server, not a hardcoded
/// one). data[0] each byte: data[0]=a, data[1]=b, data[2]=c, data[3]=d.
pub const TAG_NET_DNS: u64 = u32::from_le_bytes(*b"NDNS") as u64;
pub const TAG_UDP_BIND: u64 = u32::from_le_bytes(*b"UBND") as u64;
/// Send a datagram on a socket cap: data[0]=dst IPv4 (big-endian u32),
/// data[1]=dst port, data[2]=len, bytes from offset 24. Reply: data[0]=status.
pub const TAG_UDP_SENDTO: u64 = u32::from_le_bytes(*b"USND") as u64;
/// Receive a datagram on a socket cap (blocks server-side until one arrives for
/// the bound port). Reply: data[0]=len, payload bytes from offset 8 (<=56).
pub const TAG_UDP_RECVFROM: u64 = u32::from_le_bytes(*b"URCV") as u64;

// TCP, backed by smoltcp (§23). Same capability shape as UDP: connect on the
// NET_CTL cap mints a badged TCP-socket cap; send/recv/close ride that cap.
/// Connect: request on NET_CTL, data[0]=dst IPv4 (BE u32), data[1]=dst port.
/// Reply: data[0]=status (0=ok), handles[0]=badged TCP-socket cap.
pub const TAG_TCP_CONNECT: u64 = u32::from_le_bytes(*b"TCON") as u64;
/// Send on a TCP socket cap: data[0]=len, bytes from offset 8 (<=48).
/// Reply: data[0]=status.
pub const TAG_TCP_SEND: u64 = u32::from_le_bytes(*b"TSND") as u64;
/// Receive on a TCP socket cap (blocks server-side until data or close).
/// Reply: data[0]=len (0=closed), payload bytes from offset 8 (<=56).
pub const TAG_TCP_RECV: u64 = u32::from_le_bytes(*b"TRCV") as u64;
/// Close a TCP socket cap. Reply: data[0]=status.
pub const TAG_TCP_CLOSE: u64 = u32::from_le_bytes(*b"TCLO") as u64;

// --- Large UDP via a shared frame (§25) ------------------------------------
// The inline UDP path caps a datagram at ~40 bytes — too small for DNS with
// EDNS or multi-record answers (and c-ares). A client shares a page-sized Frame
// with the net server (TAG_UDP_ATTACH on NET_CTL); thereafter SENDV writes the
// datagram FROM that shared page and RECVV writes a received datagram TO it, so a
// full <=1472-byte UDP payload moves in one IPC. (One shared frame per net
// client; one resolver at a time.)
/// Where the net server maps the client's shared UDP transfer frame.
pub const NET_SHARED: u64 = 0x4050_0000;
/// ATTACH (NET_CTL): handles[0] = a writable Frame; net maps it as the shared
/// UDP buffer. Reply: data[0]=status (0 ok).
pub const TAG_UDP_ATTACH: u64 = u32::from_le_bytes(*b"UATT") as u64;
/// SENDV (udp socket cap): data[0]=dst IPv4 (BE u32), data[1]=dst port, data[2]=
/// len. Net sends `len` bytes from the shared frame. Reply: data[0]=status.
pub const TAG_UDP_SENDV: u64 = u32::from_le_bytes(*b"USNV") as u64;
/// RECVV (udp socket cap): non-blocking; net writes the next datagram for the
/// bound port INTO the shared frame. Reply: data[0]=len (0 = none buffered).
pub const TAG_UDP_RECVV: u64 = u32::from_le_bytes(*b"URVB") as u64;
/// CLOSE (udp socket cap): free the net server's socket slot. Without this, every
/// bind permanently consumes one of the (few) socket slots — c-ares opens/retries
/// many UDP sockets per resolve, which starved later TCP connects. Reply: status.
pub const TAG_UDP_CLOSE: u64 = u32::from_le_bytes(*b"UCLO") as u64;

/// `sys_spawn` grant convention: the handles in the spawn MsgBuf land in the
/// child's table at these slots, in order (HANDLE_NULL entries are skipped).
/// Slot 3 is always the child's fresh Memory budget, so it is not in this list.
/// The 4th slot is `BOOT_NET_EP` (20) so a spawner can pass network access to a
/// child (used by the BSD-sockets libc + curl); the previous spare (5) was unused.
pub const SPAWN_SLOTS: [Handle; 4] = [1, 2, 4, BOOT_NET_EP];
/// A spawned program's standard output endpoint (a tty R_SEND endpoint) — the
/// parent passes it as the 2nd grant so it lands here. Mirrors BOOT_CONSOLE's
/// number, so programs that printed via BOOT_CONSOLE need no slot change.
pub const SPAWN_STDOUT: Handle = 2;
/// Child Memory budget if the spawn MsgBuf requests 0 (256 KiB).
pub const SPAWN_DEFAULT_BUDGET: u64 = 64 * 4096;
/// The argument string (NUL-terminated, <=55 bytes) rides in the spawn MsgBuf's
/// `data[1..]`; the kernel maps a page at this vaddr in the child and writes it
/// there. A spawned program reads its argument from `SPAWN_ARGV` (`rt::argv()`).
pub const SPAWN_ARGV: u64 = 0x0F00_0000;

/// The caller's IDENTITY record (`IdentRec`) rides in the spawn MsgBuf's
/// `data[3]` (pointer) / `data[4]` (length); the kernel maps a read-only page at
/// this vaddr in the child and copies it there. A program reads it via
/// `rt::identity()`. §24 (capability-native identity): identity is DESCRIPTIVE —
/// it names who you are (uid/name/home for `whoami`, `getpwnam`, POSIX compat)
/// and grants NOTHING. Authority remains the capabilities you hold (law L1). A
/// parent may set any identity for its child; that is harmless precisely because
/// identity confers no access. The page is zeroed when no identity is passed,
/// which `rt` reads as root (uid 0, home "/").
pub const SPAWN_IDENT: u64 = 0x0F00_1000;

/// Max bytes for the username and home-path fields of [`IdentRec`] (NUL-padded).
pub const IDENT_NAME_MAX: usize = 32;
pub const IDENT_HOME_MAX: usize = 128;
/// Max supplementary groups carried in an [`IdentRec`].
pub const IDENT_GROUPS_MAX: usize = 16;

/// The inherited identity record (see [`SPAWN_IDENT`]). Fixed layout so kernel,
/// rt, and libc agree byte-for-byte; ~236 bytes, well under a page. Purely
/// descriptive — holding it grants no authority.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IdentRec {
    pub uid: u32,
    pub gid: u32,
    /// Valid entries in `groups` (`0..=IDENT_GROUPS_MAX`).
    pub ngroups: u32,
    pub groups: [u32; IDENT_GROUPS_MAX],
    /// Login name, NUL-padded (empty = "root" when uid==0).
    pub name: [u8; IDENT_NAME_MAX],
    /// Home directory path, NUL-padded (empty = "/").
    pub home: [u8; IDENT_HOME_MAX],
}

impl IdentRec {
    pub const fn zeroed() -> Self {
        IdentRec {
            uid: 0,
            gid: 0,
            ngroups: 0,
            groups: [0; IDENT_GROUPS_MAX],
            name: [0; IDENT_NAME_MAX],
            home: [0; IDENT_HOME_MAX],
        }
    }

    /// Build a record from parts (truncating over-long name/home, NUL-padded).
    pub fn new(uid: u32, gid: u32, name: &[u8], home: &[u8]) -> Self {
        let mut r = Self::zeroed();
        r.uid = uid;
        r.gid = gid;
        r.set_name(name);
        r.set_home(home);
        r
    }

    pub fn set_name(&mut self, s: &[u8]) {
        copy_field(&mut self.name, s);
    }

    pub fn set_home(&mut self, s: &[u8]) {
        copy_field(&mut self.home, s);
    }

    /// Append a supplementary group (ignored once `IDENT_GROUPS_MAX` is reached).
    pub fn add_group(&mut self, gid: u32) {
        let n = self.ngroups as usize;
        if n < IDENT_GROUPS_MAX {
            self.groups[n] = gid;
            self.ngroups += 1;
        }
    }

    /// The login name as bytes, trimmed at the first NUL.
    pub fn name_bytes(&self) -> &[u8] {
        let n = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        &self.name[..n]
    }

    /// The home path as bytes, trimmed at the first NUL.
    pub fn home_bytes(&self) -> &[u8] {
        let n = self.home.iter().position(|&b| b == 0).unwrap_or(self.home.len());
        &self.home[..n]
    }
}

/// Copy `src` into a fixed NUL-padded byte field, truncating if too long.
fn copy_field(dst: &mut [u8], src: &[u8]) {
    let n = core::cmp::min(dst.len(), src.len());
    dst[..n].copy_from_slice(&src[..n]);
    for b in &mut dst[n..] {
        *b = 0;
    }
}

/// "PING" — request tag for the v0 roundtrip.
pub const TAG_PING: u64 = 0x474E4950;
/// "PONG" — reply tag for the v0 roundtrip.
pub const TAG_PONG: u64 = 0x474E4F50;
/// "SHMM" — a Frame capability rides this message (shared-memory demo).
pub const TAG_SHMEM: u64 = 0x4D4D4853;
/// "NTFY" — a Notification capability rides this message.
pub const TAG_NOTIF: u64 = 0x5946544E;
/// TTY protocol tags: kbd→tty a character; shell↔tty read a line; shell→tty output.
pub const TAG_TTY_CHAR: u64 = 0x52414843; // "CHAR"
pub const TAG_TTY_READ: u64 = 0x44414552; // "READ"
pub const TAG_TTY_LINE: u64 = 0x454E494C; // "LINE"
pub const TAG_TTY_WRITE: u64 = 0x54495257; // "WRIT"
