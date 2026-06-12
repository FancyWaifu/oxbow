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

// Notifications reuse the IPC verbs: signalling is "send", waiting is "recv".
pub const R_SIGNAL: u32 = R_SEND;
pub const R_WAIT: u32 = R_RECV;

// Mapping protection flags for sys_map / sys_frame_map (NOT rights; per call).
// Exec is intentionally absent — W^X forbids writable+executable, and there is
// no mprotect yet, so an executable anonymous page would be useless.
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2; // implies read

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
}

impl MsgBuf {
    /// An empty message with the given tag and no payload or handles.
    pub const fn new(tag: u64) -> Self {
        MsgBuf {
            tag,
            data_len: 0,
            handle_count: 0,
            data: [0; MSG_DATA_WORDS],
            handles: [HANDLE_NULL; MSG_HANDLES],
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
