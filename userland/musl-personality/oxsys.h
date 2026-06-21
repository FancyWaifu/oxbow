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
#define OX_SYS_UPTIME_MS     25
#define OX_SYS_PROTECT       26
#define OX_SYS_GETENTROPY    28
#define OX_SYS_WALLTIME      52
#define OX_SYS_FUTEX_WAIT    55
#define OX_SYS_FUTEX_WAKE    56
#define OX_SYS_THREAD_ID     57
#define OX_SYS_YIELD         58
#define OX_SYS_SET_FSBASE    63

static __inline long ox_syscall0(long n)
{
	unsigned long r;
	__asm__ __volatile__("syscall" : "=a"(r) : "a"(n) : "rcx", "r11", "memory");
	return r;
}
static __inline long ox_syscall1(long n, long a1)
{
	unsigned long r;
	__asm__ __volatile__("syscall" : "=a"(r) : "a"(n), "D"(a1) : "rcx", "r11", "memory");
	return r;
}
static __inline long ox_syscall2(long n, long a1, long a2)
{
	unsigned long r;
	__asm__ __volatile__("syscall" : "=a"(r) : "a"(n), "D"(a1), "S"(a2) : "rcx", "r11", "memory");
	return r;
}
static __inline long ox_syscall3(long n, long a1, long a2, long a3)
{
	unsigned long r;
	__asm__ __volatile__("syscall" : "=a"(r) : "a"(n), "D"(a1), "S"(a2), "d"(a3)
	                     : "rcx", "r11", "memory");
	return r;
}
static __inline long ox_syscall4(long n, long a1, long a2, long a3, long a4)
{
	unsigned long r;
	register long r10 __asm__("r10") = a4;
	__asm__ __volatile__("syscall" : "=a"(r) : "a"(n), "D"(a1), "S"(a2), "d"(a3), "r"(r10)
	                     : "rcx", "r11", "memory");
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

#endif
