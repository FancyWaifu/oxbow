/* oxbow override of musl's arch/x86_64/syscall_arch.h.
 *
 * Stock musl issues `__asm__("syscall")` here. On oxbow we instead route every
 * syscall through the userland personality dispatcher __oxbow_syscall(), so no real
 * `syscall` instruction is executed by ported code and the Linux ABI is translated
 * to oxbow capabilities in userland.
 *
 * Install: copy this over musl-1.2.5/arch/x86_64/syscall_arch.h before building
 * musl (see build-musl.sh). Everything else in musl stays stock. */
#define __SYSCALL_LL_E(x) (x)
#define __SYSCALL_LL_O(x) (x)

long __oxbow_syscall(long n, long a1, long a2, long a3, long a4, long a5, long a6);

static __inline long __syscall0(long n)
{
	return __oxbow_syscall(n, 0, 0, 0, 0, 0, 0);
}
static __inline long __syscall1(long n, long a1)
{
	return __oxbow_syscall(n, a1, 0, 0, 0, 0, 0);
}
static __inline long __syscall2(long n, long a1, long a2)
{
	return __oxbow_syscall(n, a1, a2, 0, 0, 0, 0);
}
static __inline long __syscall3(long n, long a1, long a2, long a3)
{
	return __oxbow_syscall(n, a1, a2, a3, 0, 0, 0);
}
static __inline long __syscall4(long n, long a1, long a2, long a3, long a4)
{
	return __oxbow_syscall(n, a1, a2, a3, a4, 0, 0);
}
static __inline long __syscall5(long n, long a1, long a2, long a3, long a4, long a5)
{
	return __oxbow_syscall(n, a1, a2, a3, a4, a5, 0);
}
static __inline long __syscall6(long n, long a1, long a2, long a3, long a4, long a5, long a6)
{
	return __oxbow_syscall(n, a1, a2, a3, a4, a5, a6);
}

#define VDSO_USEFUL
#define IPC_64 0
