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

#endif
