//! oxbow-rt — the userland runtime for oxbow servers.
//!
//! Provides `_start` (the ELF entry), typed `syscall` stubs for the whole v0
//! ABI, and a userland panic handler. A server crate links this, defines
//! `oxbow_main() -> !`, and gets a working ring-3 runtime. See docs/abi-v0.md.
#![no_std]

use core::panic::PanicInfo;

// Re-exported so servers can `use oxbow_rt::abi` for the shared ABI types.
pub use oxbow_abi as abi;

use oxbow_abi::{
    Handle, MsgBuf, SysError, SysResult, BOOT_CONSOLE, SYS_ATTENUATE, SYS_CALL, SYS_CLOSE,
    SYS_CONSOLE_WRITE, SYS_EXIT, SYS_FRAME_ALLOC, SYS_FRAME_MAP, SYS_IO_IN, SYS_IO_OUT, SYS_IRQ_ACK,
    SYS_IRQ_BIND, SYS_MAP, SYS_NOTIF_CREATE, SYS_NOTIF_SIGNAL, SYS_NOTIF_WAIT, SYS_RECV, SYS_REPLY,
    SYS_SEND, SYS_EP_CREATE, SYS_MINT, SYS_SPAWN,
};

// --- The server provides this; _start calls it ---------------------------
extern "C" {
    fn oxbow_main() -> !;
}

/// ELF entry point. The kernel enters here at CPL 3 with `rsp` at the top of the
/// stack page. Align the stack, call into the server, and (defensively) exit if
/// it ever returns.
#[unsafe(naked)]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "and rsp, -16",   // 16-byte align (call then makes it %16==8, per SysV)
        "call {main}",
        "ud2",            // oxbow_main is -> !, so this is unreachable
        main = sym start_rust,
    );
}

extern "C" fn start_rust() -> ! {
    unsafe { oxbow_main() }
}

// --- Raw syscall stubs ----------------------------------------------------
// nr in rax; args rdi, rsi, rdx, r10, r8, r9; returns rax (+ rdx). rcx/r11 are
// clobbered by the `syscall` instruction. No `nomem`/`nostack` options: the
// kernel reads/writes user memory (MsgBuf) through these, so the compiler must
// treat them as full memory barriers.

#[inline]
unsafe fn syscall1(nr: u64, a1: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        lateout("rdx") rdx,
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

#[inline]
unsafe fn syscall2(nr: u64, a1: u64, a2: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        in("rsi") a2,
        lateout("rdx") rdx,
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

#[inline]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        in("rsi") a2,
        inlateout("rdx") a3 => rdx,
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

#[inline]
unsafe fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        in("rsi") a2,
        inlateout("rdx") a3 => rdx,
        in("r10") a4, // 4th arg per the kernel's SysV-with-r10 convention
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

// --- Typed ABI ------------------------------------------------------------

pub fn sys_send(ep: Handle, msg: *const MsgBuf) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_SEND, ep as u64, msg as u64) };
    SysError::from_raw(rax)
}

pub fn sys_recv(ep: Handle, msg: *mut MsgBuf) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall2(SYS_RECV, ep as u64, msg as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

pub fn sys_call(ep: Handle, msg: *mut MsgBuf) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_CALL, ep as u64, msg as u64) };
    SysError::from_raw(rax)
}

pub fn sys_reply(reply: Handle, msg: *const MsgBuf) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_REPLY, reply as u64, msg as u64) };
    SysError::from_raw(rax)
}

pub fn sys_attenuate(src: Handle, new_rights: u32) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall2(SYS_ATTENUATE, src as u64, new_rights as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// Mint a BADGED capability to the endpoint `src` (§14): the kernel delivers
/// `badge` to whoever receives a message sent through the returned handle.
/// `src` must be unbadged + held with R_ATTENUATE; `new_rights` ⊆ src; badge != 0.
pub fn sys_mint(src: Handle, badge: u64, new_rights: u32) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall3(SYS_MINT, src as u64, badge, new_rights as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

pub fn sys_close(h: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall1(SYS_CLOSE, h as u64) };
    SysError::from_raw(rax)
}

pub fn sys_console_write(con: Handle, buf: *const u8, len: usize) -> SysResult {
    let (rax, _) = unsafe { syscall3(SYS_CONSOLE_WRITE, con as u64, buf as u64, len as u64) };
    SysError::from_raw(rax)
}

/// Raw syscall escape hatch returning `(rax, rdx)` — for the selftest harness to
/// invoke arbitrary/unknown syscall numbers. Normal code uses the typed wrappers.
pub fn sys_raw(nr: u64, a1: u64, a2: u64, a3: u64) -> (u64, u64) {
    unsafe { syscall3(nr, a1, a2, a3) }
}

pub fn sys_map(mem: Handle, vaddr: u64, len: u64, prot: u64) -> SysResult {
    let (rax, _) = unsafe { syscall4(SYS_MAP, mem as u64, vaddr, len, prot) };
    SysError::from_raw(rax)
}

pub fn sys_frame_alloc(mem: Handle) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(SYS_FRAME_ALLOC, mem as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

pub fn sys_frame_map(frame: Handle, vaddr: u64, prot: u64) -> SysResult {
    let (rax, _) = unsafe { syscall3(SYS_FRAME_MAP, frame as u64, vaddr, prot) };
    SysError::from_raw(rax)
}

pub fn sys_notif_create() -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(SYS_NOTIF_CREATE, 0) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// Spawn a program `image` into a new process. `mem` (a Memory budget) pays;
/// `msg` carries the child budget (`data[0]`) and the capabilities to grant it
/// (`handles`, per the §13 slot convention); `exit_notif` is signalled when the
/// child exits (or HANDLE_NULL for fire-and-forget). Returns the child pid.
pub fn sys_spawn(
    image: Handle,
    mem: Handle,
    msg: *const MsgBuf,
    exit_notif: Handle,
) -> SysResult<u64> {
    let (rax, rdx) =
        unsafe { syscall4(SYS_SPAWN, image as u64, mem as u64, msg as u64, exit_notif as u64) };
    SysError::from_raw(rax).map(|_| rdx)
}

/// Mint a fresh Endpoint (R_SEND|R_RECV|R_GRANT|R_ATTENUATE) — for a parent to
/// set up an IPC channel between the children it spawns.
pub fn sys_ep_create() -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(SYS_EP_CREATE, 0) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

pub fn sys_notif_signal(notif: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall1(SYS_NOTIF_SIGNAL, notif as u64) };
    SysError::from_raw(rax)
}

/// Block until the notification is signalled; returns the latched signal count.
pub fn sys_notif_wait(notif: Handle) -> SysResult<u64> {
    let (rax, rdx) = unsafe { syscall1(SYS_NOTIF_WAIT, notif as u64) };
    SysError::from_raw(rax).map(|_| rdx)
}

pub fn sys_io_in(ioport: Handle, port: u16) -> SysResult<u8> {
    let (rax, rdx) = unsafe { syscall2(SYS_IO_IN, ioport as u64, port as u64) };
    SysError::from_raw(rax).map(|_| rdx as u8)
}

pub fn sys_io_out(ioport: Handle, port: u16, value: u8) -> SysResult {
    let (rax, _) = unsafe { syscall3(SYS_IO_OUT, ioport as u64, port as u64, value as u64) };
    SysError::from_raw(rax)
}

pub fn sys_irq_bind(irq: Handle, notif: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_IRQ_BIND, irq as u64, notif as u64) };
    SysError::from_raw(rax)
}

pub fn sys_irq_ack(irq: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall1(SYS_IRQ_ACK, irq as u64) };
    SysError::from_raw(rax)
}

pub fn sys_exit(code: u64) -> ! {
    unsafe {
        syscall1(SYS_EXIT, code);
    }
    // sys_exit never returns; the kernel halts the process. Loop as a backstop.
    loop {
        core::hint::spin_loop();
    }
}

// --- Panic handler --------------------------------------------------------

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // Best-effort: announce on the console capability, then exit non-zero.
    let msg = b"pong: panic\n";
    let _ = sys_console_write(BOOT_CONSOLE, msg.as_ptr(), msg.len());
    sys_exit(101)
}
