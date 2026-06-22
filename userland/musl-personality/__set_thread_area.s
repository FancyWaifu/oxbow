/* oxbow personality override of musl's x86_64 __set_thread_area.
 *
 * Upstream issues a RAW `syscall` with the Linux arch_prctl(ARCH_SET_FS) number
 * (eax=158). That bypasses the personality's syscall_arch.h override (which only
 * redirects musl's C __syscallN), so installing musl's thread pointer went straight
 * to the oxbow kernel as an unrecognized Linux syscall and was dropped — `fs` stayed
 * on the kernel's bare TLS block (no pthread struct, locale=NULL), and the first
 * locale-touching call (setlocale / MB_CUR_MAX) faulted on a NULL deref.
 *
 * oxbow sets the calling thread's FS base via SYS_SET_FSBASE (=63): rax=63,
 * rdi=base. The thread pointer arrives in rdi already, so we just load the number
 * and issue the oxbow syscall. Returns 0. */
.text
.global __set_thread_area
.hidden __set_thread_area
.type __set_thread_area,@function
__set_thread_area:
	movl $63,%eax           /* oxbow SYS_SET_FSBASE */
	syscall                 /* rdi = p (the thread pointer) */
	xorl %eax,%eax
	ret
