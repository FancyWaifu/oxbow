/* oxbow raw syscall ABI for the POSIX/Linux personality.
 *
 * oxbow's userland syscall convention is SysV-with-r10 — the SAME register layout
 * as Linux x86_64 (nr in rax; args in rdi, rsi, rdx, r10, r8, r9; returns rax, and
 * rdx for the few syscalls that produce a second value). So we can issue oxbow
 * syscalls from C with the exact inline-asm shape musl uses for Linux. */
#ifndef OXBOW_OXSYS_H
#define OXBOW_OXSYS_H

/* oxbow syscall numbers (must track abi/src/lib.rs). Only the ones the personality
 * issues directly are listed. */
#define OX_SYS_CONSOLE_WRITE 6
#define OX_SYS_EXIT          7
#define OX_SYS_MAP           8
#define OX_SYS_NOTIF_CREATE  11
#define OX_SYS_UPTIME_MS     25
#define OX_SYS_PROTECT       26
#define OX_SYS_GETENTROPY    28
#define OX_SYS_WALLTIME      52
#define OX_SYS_FUTEX_WAIT    55
#define OX_SYS_FUTEX_WAKE    56
#define OX_SYS_THREAD_ID     57
#define OX_SYS_YIELD         58
#define OX_SYS_SET_FSBASE    63
#define OX_SYS_FORK          64
#define OX_SYS_SIGDISPATCH   67
#define OX_SYS_SIGRETURN     68
/* §wayland: the channel + shm primitives the Wayland transport rides on. A channel
 * cap carries BOTH bytes and capability handles (SCM_RIGHTS); shm regions back the
 * wl_shm pixel buffers. Numbers track abi/src/lib.rs. */
#define OX_SYS_CHANNEL_SEND  38  /* (h, buf, len, caps_ptr, ncaps) -> rdx = nbytes */
#define OX_SYS_CHANNEL_RECV  39  /* (h, buf, len, caps_out, ncaps_max|flags<<32) -> rdx = nbytes|ncaps<<32 */
#define OX_SYS_CHANNEL_POLL  41  /* (h) -> rdx readiness: 1=readable 2=eof 4=writable */
#define OX_SYS_SHM_CREATE    42  /* (mem, pages) -> rdx = Shm handle */
#define OX_SYS_SHM_MAP       43  /* (shm, vaddr) -> rdx = bytes mapped */
#define OX_BOOT_MEM          3   /* the Memory cap handle granted at spawn */
#define OX_CHAN_NONBLOCK     1   /* CHANNEL_RECV flag: don't block when empty */
/* §102 pseudo-terminals — the kernel runs the line discipline (kernel/src/pty.rs). */
#define OX_SYS_PTY_CREATE    70  /* () -> rdx = master | slave<<32 */
#define OX_SYS_PTY_READ      71  /* (h, buf, len) -> rdx count (0=EOF) */
#define OX_SYS_PTY_WRITE     72  /* (h, buf, len) -> rdx count */
#define OX_SYS_PTY_IOCTL     73  /* (h, op, arg) -> rdx (op 0x100 = poll readiness) */

/* CRITICAL: oxbow syscalls return TWO values (rax + RDX), unlike Linux (rax only).
 * So RDX is ALWAYS clobbered by the syscall — it must be an asm output, never left
 * as an unmentioned register the compiler thinks survives. (A subtle miss here let
 * the compiler reuse a now-zeroed rdx as a live variable across SYS_FORK.) Helpers
 * that pass an arg in rdx (a3) use "+d" (in-out); the rest capture+discard it. */
static __inline long ox_syscall0(long n)
{
	unsigned long r, d;
	__asm__ __volatile__("syscall" : "=a"(r), "=d"(d) : "a"(n) : "rcx", "r11", "memory");
	(void)d;
	return r;
}
/* sys_notif_create returns the handle in RDX (status in RAX, 0=ok). Create a
 * Notification and return its handle, or -1 on failure. */
static __inline long ox_notif_create(void)
{
	unsigned long rax, rdx;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "=d"(rdx)
	                     : "a"((long)OX_SYS_NOTIF_CREATE)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : -1;
}
static __inline long ox_syscall1(long n, long a1)
{
	unsigned long r, d;
	__asm__ __volatile__("syscall" : "=a"(r), "=d"(d) : "a"(n), "D"(a1) : "rcx", "r11", "memory");
	(void)d;
	return r;
}
static __inline long ox_syscall2(long n, long a1, long a2)
{
	unsigned long r, d;
	__asm__ __volatile__("syscall"
	                     : "=a"(r), "=d"(d)
	                     : "a"(n), "D"(a1), "S"(a2)
	                     : "rcx", "r11", "memory");
	(void)d;
	return r;
}
static __inline long ox_syscall3(long n, long a1, long a2, long a3)
{
	unsigned long r, d = (unsigned long)a3; /* rdx: in (a3) AND clobbered out */
	__asm__ __volatile__("syscall"
	                     : "=a"(r), "+d"(d)
	                     : "a"(n), "D"(a1), "S"(a2)
	                     : "rcx", "r11", "memory");
	(void)d;
	return r;
}
static __inline long ox_syscall4(long n, long a1, long a2, long a3, long a4)
{
	unsigned long r, d = (unsigned long)a3;
	register long r10 __asm__("r10") = a4;
	__asm__ __volatile__("syscall"
	                     : "=a"(r), "+d"(d)
	                     : "a"(n), "D"(a1), "S"(a2), "r"(r10)
	                     : "rcx", "r11", "memory");
	(void)d;
	return r;
}

/* §wayland channel/shm primitives. These take 5 args and/or read RDX as a real
 * return value, so they're hand-rolled (the generic ox_syscallN discard RDX). */

/* Send `len` bytes + `ncaps` capability handles over channel `h`. Returns bytes sent
 * (0 if the peer is gone). The caps are the SCM_RIGHTS fd-passing of shm buffers. */
static __inline long ox_chan_send(unsigned int h, const void *buf, unsigned long len,
                                  const unsigned int *caps, unsigned long ncaps)
{
	unsigned long rax, rdx = len;
	register long r10 __asm__("r10") = (long)caps;
	register long r8 __asm__("r8") = (long)ncaps;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "+d"(rdx)
	                     : "a"((long)OX_SYS_CHANNEL_SEND), "D"((long)h), "S"((long)buf),
	                       "r"(r10), "r"(r8)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : 0;
}

/* Receive up to `len` bytes + up to `ncaps_max` caps over channel `h`. With nonblock,
 * returns -1 if nothing is buffered. On success returns nbytes|ncaps<<32 (split by the
 * caller). The Wayland display fd is always O_NONBLOCK + poll-driven. */
static __inline long ox_chan_recv(unsigned int h, void *buf, unsigned long len,
                                  unsigned int *caps, unsigned long ncaps_max, int nonblock)
{
	unsigned long rax, rdx = len;
	unsigned long packed = ncaps_max |
	    ((unsigned long)(nonblock ? OX_CHAN_NONBLOCK : 0) << 32);
	register long r10 __asm__("r10") = (long)caps;
	register long r8 __asm__("r8") = (long)packed;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "+d"(rdx)
	                     : "a"((long)OX_SYS_CHANNEL_RECV), "D"((long)h), "S"((long)buf),
	                       "r"(r10), "r"(r8)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : -1;
}

/* Channel readiness bits (1=readable 2=eof 4=writable) — for poll() on the display fd. */
static __inline long ox_chan_poll(unsigned int h)
{
	unsigned long rax, rdx;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "=d"(rdx)
	                     : "a"((long)OX_SYS_CHANNEL_POLL), "D"((long)h)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : 0;
}

/* Allocate an shm region of `pages` 4 KiB frames from BOOT_MEM; returns its cap handle
 * (>=0) or -1. Backs memfd_create + ftruncate for wl_shm pools. */
static __inline long ox_shm_create(unsigned long pages)
{
	unsigned long rax, rdx = pages;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "+d"(rdx)
	                     : "a"((long)OX_SYS_SHM_CREATE), "D"((long)OX_BOOT_MEM), "S"(pages)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : -1;
}

/* Map shm region `h` at `vaddr` (RW); returns bytes mapped, or 0 on failure. */
static __inline long ox_shm_map(unsigned int h, unsigned long vaddr)
{
	unsigned long rax, rdx = vaddr;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "+d"(rdx)
	                     : "a"((long)OX_SYS_SHM_MAP), "D"((long)h), "S"(vaddr)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : 0;
}

/* §pty: create a pty; returns the master cap (>=0), writes the slave cap to *slave. */
static __inline long ox_pty_create(unsigned int *slave)
{
	unsigned long rax, rdx;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "=d"(rdx)
	                     : "a"((long)OX_SYS_PTY_CREATE)
	                     : "rcx", "r11", "memory");
	if (rax != 0)
		return -1;
	if (slave)
		*slave = (unsigned int)(rdx >> 32);
	return (long)(rdx & 0xffffffffUL);
}
static __inline long ox_pty_read(unsigned int h, void *buf, unsigned long len)
{
	unsigned long rax, rdx = len;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "+d"(rdx)
	                     : "a"((long)OX_SYS_PTY_READ), "D"((long)h), "S"((long)buf)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : -1;
}
static __inline long ox_pty_write(unsigned int h, const void *buf, unsigned long len)
{
	unsigned long rax, rdx = len;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "+d"(rdx)
	                     : "a"((long)OX_SYS_PTY_WRITE), "D"((long)h), "S"((long)buf)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : -1;
}
/* op 0x100 = poll readiness (returns 1 if readable); else TCGETS/TCSETS/TIOC* with the
 * user struct at `arg`. Returns rdx (or -1 on error). */
static __inline long ox_pty_ioctl(unsigned int h, unsigned long op, unsigned long arg)
{
	unsigned long rax, rdx = arg;
	__asm__ __volatile__("syscall"
	                     : "=a"(rax), "+d"(rdx)
	                     : "a"((long)OX_SYS_PTY_IOCTL), "D"((long)h), "S"(op)
	                     : "rcx", "r11", "memory");
	return rax == 0 ? (long)rdx : -1;
}

/* IPC-backed primitives provided by oxbow-rt (feature = "hosted"): stdout/stderr
 * go through the tty/pipe path, not a raw syscall; anonymous mmap and the TLS base
 * are thin wrappers. */
extern long  __oxbow_write(int fd, const void *buf, unsigned long len);
extern long  __oxbow_read(int fd, void *buf, unsigned long len);
extern void *__oxbow_mmap_anon(unsigned long len);
extern void  __oxbow_set_fsbase(unsigned long addr);

/* fsd-backed file ops (Phase 2). open resolves the path against the process's cwd
 * dir cap (the namespace); pread/pwrite are offset-based over the file capability.
 * open returns the file cap (>=0) or a negative status (-1 NotFound, -2 Exists). */
extern long __oxbow_fs_open(const char *path, unsigned long len, unsigned int flags,
                            unsigned long *size_out, int *kind_out,
                            unsigned int *mtime_out, unsigned int *atime_out);
extern long __oxbow_fs_pread(long file, void *buf, unsigned long len, unsigned long off);
extern long __oxbow_fs_pwrite(long file, const void *buf, unsigned long len, unsigned long off);
extern void __oxbow_fs_close(long file);
extern int  __oxbow_fs_truncate(long file, unsigned long size);
extern long __oxbow_fs_readdir(long dir, unsigned long cursor, unsigned char *name_out,
                               unsigned long name_cap, unsigned int *kind_out);

/* process spawn/wait (Phase 3), reusing the std::process::Command shims. spawn
 * loads `elf` via SYS_SPAWN_BYTES inheriting cwd+stdout, returns the exit-notif
 * handle (>=0) + writes the child pid; wait blocks on the notif and returns the
 * exit status. */
extern long __oxbow_spawn(const void *elf, unsigned long elf_len, const void *argv,
                          unsigned long argv_len, unsigned int stdout_cap,
                          unsigned int stdin_cap, unsigned int *pid_out);
extern int  __oxbow_wait(long notif);

/* pipes (Phase 6): __oxbow_pipe creates a pair, writing the read/write endpoint
 * handles; read/write/close/dup/eof operate on an endpoint handle. Back pipe()/
 * dup2() and a pipeline's inherited stdin. */
extern int  __oxbow_pipe(unsigned int *rend_out, unsigned int *wend_out);
extern long __oxbow_pipe_read(unsigned int pipe, void *buf, unsigned long len);
extern long __oxbow_pipe_write(unsigned int pipe, const void *buf, unsigned long len);
extern void __oxbow_pipe_close(unsigned int pipe);
extern long __oxbow_pipe_dup(unsigned int pipe);
extern void __oxbow_pipe_eof(unsigned int pipe);

/* tty line discipline (Phase 7): raw != 0 -> raw keystroke delivery (TUI apps with
 * ~ICANON), 0 -> cooked. Sent on tcsetattr based on the ICANON flag. */
extern void __oxbow_tty_mode(int raw);

/* BSD sockets (Phase 1: TCP client). Map Linux socket()/connect()/send/recv onto
 * oxbow's capability TCP API via the session net cap. `ip` is packed a<<24|b<<16|c<<8|d
 * (dotted-quad order); `port` is host order. connect returns a socket cap (>=0) or -1. */
extern long __oxbow_sock_tcp_connect(unsigned int ip, unsigned short port);
extern long __oxbow_sock_send(long sock, const void *buf, unsigned long len);
extern long __oxbow_sock_recv(long sock, void *buf, unsigned long len);
extern long __oxbow_sock_recv_nb(long sock, void *buf, unsigned long len);
extern void __oxbow_sock_close(long sock);

/* TCP server path: listen returns a listener cap; accept (non-blocking) returns a fresh
 * connected socket cap + the peer IPv4 (packed a<<24|… dotted order) + port, or -1 when
 * nothing is pending (the personality polls with a yield to block). */
extern long __oxbow_sock_tcp_listen(unsigned short port);
extern long __oxbow_sock_tcp_accept(long listener, unsigned int *peer_ip,
                                    unsigned short *peer_port);

/* UDP (Phase 2: powers musl's DNS resolver). bind returns a socket cap; sendto/recvfrom
 * carry the peer address. recvfrom writes the sender IPv4 (packed a<<24|… dotted order)
 * + port so the resolver can validate the reply source. */
extern long __oxbow_sock_udp_bind(unsigned short port);
extern long __oxbow_sock_udp_sendto(long sock, unsigned int ip, unsigned short port,
                                    const void *buf, unsigned long len);
extern long __oxbow_sock_udp_recvfrom(long sock, void *buf, unsigned long len,
                                      unsigned int *src_ip, unsigned short *src_port);
extern void __oxbow_sock_udp_close(long sock);

#endif
