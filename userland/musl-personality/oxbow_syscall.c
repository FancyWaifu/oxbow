/* The oxbow POSIX/Linux personality: translate the Linux x86_64 syscall ABI that
 * musl issues into oxbow capability operations.
 *
 * musl's arch/x86_64/syscall_arch.h is overridden (see syscall_arch.h here) so every
 * __syscallN(n, ...) lands in __oxbow_syscall() below instead of issuing a real
 * `syscall` instruction. Failures return Linux-style negative errno; musl's
 * __syscall_ret maps that to errno + (-1).
 *
 * STATUS: Phase 1 (first light). Implemented well enough to start a stock-musl
 * program, run its allocator, write to stdout/stderr, and exit. Files, fork/exec,
 * and signals are stubbed (return -ENOSYS or a benign success) and are the next
 * phases — see docs/posix-personality-plan.md. */
#include "oxsys.h"
#include "linux_nr.h"

#include <stdarg.h>
#include <stdint.h>

struct ox_timespec { long tv_sec; long tv_nsec; };

/* monotonic + realtime clocks via oxbow */
static long do_clock_gettime(long id, struct ox_timespec *ts)
{
	if (!ts)
		return -E_FAULT;
	if (id == CLOCK_MONOTONIC) {
		unsigned long ms = (unsigned long)ox_syscall0(OX_SYS_UPTIME_MS);
		ts->tv_sec = ms / 1000;
		ts->tv_nsec = (ms % 1000) * 1000000L;
		return 0;
	}
	/* CLOCK_REALTIME (and anything else): RTC walltime. SYS_WALLTIME returns
	 * epoch secs in rax and nanoseconds in rdx; we only see rax here, so nsec=0
	 * for now (sub-second precision is a later refinement). */
	long secs = ox_syscall0(OX_SYS_WALLTIME);
	ts->tv_sec = secs;
	ts->tv_nsec = 0;
	return 0;
}

long __oxbow_syscall(long n, long a1, long a2, long a3, long a4, long a5, long a6)
{
	(void)a5;
	(void)a6;
	switch (n) {

	/* ---- output / input ---- */
	case NR_write:
		/* fd 1/2 (and, for now, everything) -> the rt stdout path (tty/pipe). */
		return __oxbow_write((int)a1, (const void *)a2, (unsigned long)a3);
	case NR_writev: {
		const struct { const void *base; unsigned long len; } *iov =
			(const void *)a2;
		long total = 0;
		for (long i = 0; i < a3; i++) {
			if (iov[i].len == 0)
				continue;
			long w = __oxbow_write((int)a1, iov[i].base, iov[i].len);
			if (w < 0)
				return total ? total : w;
			total += w;
		}
		return total;
	}
	case NR_read:
		return __oxbow_read((int)a1, (void *)a2, (unsigned long)a3);

	/* ---- memory ---- */
	case NR_mmap: {
		/* musl asks for anonymous RW pages for its allocator; we ignore the
		 * fd/offset/prot detail in Phase 1 and hand back fresh RW pages. */
		void *p = __oxbow_mmap_anon((unsigned long)a2);
		return (long)p;
	}
	case NR_munmap:
		return 0; /* no reclamation yet (Phase 1); pretend success. */
	case NR_mprotect:
		return ox_syscall4(OX_SYS_PROTECT, /*mem*/ 0, a1, a2, a3) ? -E_INVAL : 0;
	case NR_madvise:
		return 0;
	case NR_brk:
		return -E_NOSYS; /* musl's mallocng falls back to mmap. */

	/* ---- TLS / thread identity ---- */
	case NR_arch_prctl:
		if (a1 == ARCH_SET_FS) {
			__oxbow_set_fsbase((unsigned long)a2);
			return 0;
		}
		return -E_INVAL;
	case NR_set_tid_address:
	case NR_gettid:
	case NR_getpid:
		return ox_syscall0(OX_SYS_THREAD_ID);

	/* ---- scheduling / time / entropy ---- */
	case NR_sched_yield:
		return ox_syscall0(OX_SYS_YIELD);
	case NR_clock_gettime:
		return do_clock_gettime(a1, (void *)a2);
	case NR_getrandom:
		return ox_syscall2(OX_SYS_GETENTROPY, a1, a2) ? -E_INVAL : (long)a2;

	/* ---- futex (musl locks + pthreads) ---- */
	case NR_futex: {
		int op = (int)a2 & 0x7f; /* strip FUTEX_PRIVATE_FLAG */
		if (op == 0 /*FUTEX_WAIT*/)
			return ox_syscall3(OX_SYS_FUTEX_WAIT, a1, a3, /*timeout*/ 0);
		if (op == 1 /*FUTEX_WAKE*/)
			return ox_syscall2(OX_SYS_FUTEX_WAKE, a1, a3);
		return -E_INVAL;
	}

	/* ---- terminal niceties (stubbed so isatty/term-size don't fault) ---- */
	case NR_ioctl:
		return -E_NOSYS;

	/* ---- signals: accept installs as no-ops so handler-installing code runs ---- */
	case NR_rt_sigaction:
	case NR_rt_sigprocmask:
		return 0;

	/* ---- single-user identity ---- */
	case NR_getuid:
	case NR_geteuid:
	case NR_getgid:
	case NR_getegid:
		return 0; /* root */

	/* ---- exit ---- */
	case NR_exit:
	case NR_exit_group:
		ox_syscall1(OX_SYS_EXIT, a1);
		__builtin_unreachable();

	/* ---- not yet: files (Phase 2), fork/exec (Phase 3) ---- */
	case NR_open:
	case NR_openat:
	case NR_close:
	case NR_stat:
	case NR_fstat:
	case NR_newfstatat:
	case NR_lseek:
	case NR_dup:
	case NR_dup2:
	case NR_fcntl:
	case NR_access:
	case NR_readlink:
	case NR_getcwd:
	case NR_uname:
	case NR_nanosleep:
	default:
		return -E_NOSYS;
	}
}
