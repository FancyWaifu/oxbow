/* The oxbow POSIX/Linux personality: translate the Linux x86_64 syscall ABI that
 * musl issues into oxbow capability operations.
 *
 * musl's arch/x86_64/syscall_arch.h is overridden (see syscall_arch.h here) so every
 * __syscallN(n, ...) lands in __oxbow_syscall() below instead of issuing a real
 * `syscall` instruction. Failures return Linux-style negative errno; musl's
 * __syscall_ret maps that to errno + (-1).
 *
 * STATUS: Phase 3b. Phase 1 (stdout/exit/alloc) + Phase 2 (fd table + open/read/
 * write/lseek/stat over fsd, paths resolved against the cwd dir cap) + Phase 3 real
 * fork (kernel AS-clone) + execve + waitpid. Signals are next (Phase 4). See
 * docs/posix-personality-plan.md. */
#include "oxsys.h"
#include "linux_nr.h"

#include <stdarg.h>
#include <stdint.h>
#include <setjmp.h>

extern void *malloc(unsigned long);
extern void free(void *);

/* fork (Phase 3b): the kernel SYS_FORK clones our AS + handle table into a new
 * process and starts its main thread at `fork_trampoline` (in the child's OWN copied
 * AS). The trampoline longjmps to the context setjmp captured in fork() — at the same
 * virtual address in the copied AS — so the child resumes at the fork point on its
 * own copied stack. No shared memory, no corruption: real fork. */
static jmp_buf fork_buf;
static char fork_child_stack[8192] __attribute__((aligned(16)));
static void fork_trampoline(void)
{
	longjmp(fork_buf, 1);
}

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
	long secs = ox_syscall0(OX_SYS_WALLTIME);
	ts->tv_sec = secs;
	ts->tv_nsec = 0;
	return 0;
}

/* ----------------------------- fd table ------------------------------------ */
#define MAXFD 64
struct oxfd {
	int used;
	long handle;         /* fsd file capability */
	unsigned long off;   /* current file offset */
	unsigned long size;  /* known size (grows on write) */
};
static struct oxfd fds[MAXFD];

static unsigned long slen(const char *s)
{
	unsigned long n = 0;
	while (s && s[n])
		n++;
	return n;
}

static int fd_alloc(long handle, unsigned long size)
{
	for (int i = 3; i < MAXFD; i++) {
		if (!fds[i].used) {
			fds[i].used = 1;
			fds[i].handle = handle;
			fds[i].off = 0;
			fds[i].size = size;
			return i;
		}
	}
	return -1;
}

static long do_open(const char *path, long flags)
{
	if (!path)
		return -E_FAULT;
	unsigned int ox = 0;
	if (flags & LO_CREAT)
		ox |= 1; /* FS_O_CREATE */
	if (flags & LO_EXCL)
		ox |= 2; /* FS_O_EXCL   */
	if (flags & LO_TRUNC)
		ox |= 4; /* FS_O_TRUNC  */

	unsigned long size = 0;
	int kind = 0;
	unsigned int mt = 0, at = 0;
	long h = __oxbow_fs_open(path, slen(path), ox, &size, &kind, &mt, &at);
	if (h < 0)
		return (h == -1) ? -E_NOENT : (h == -2) ? -E_EXIST : -E_INVAL;
	int fd = fd_alloc(h, size);
	if (fd < 0) {
		__oxbow_fs_close(h);
		return -E_MFILE;
	}
	return fd;
}

/* read/write that dispatch on fd: 0/1/2 -> the tty path, >=3 -> fsd file. */
static long do_read(long fd, void *buf, unsigned long len)
{
	if (fd < 3)
		return __oxbow_read((int)fd, buf, len);
	if (fd >= MAXFD || !fds[fd].used)
		return -E_BADF;
	long n = __oxbow_fs_pread(fds[fd].handle, buf, len, fds[fd].off);
	if (n > 0)
		fds[fd].off += (unsigned long)n;
	return n;
}

static long do_write(long fd, const void *buf, unsigned long len)
{
	if (fd == 1 || fd == 2)
		return __oxbow_write((int)fd, buf, len);
	if (fd < 3 || fd >= MAXFD || !fds[fd].used)
		return -E_BADF;
	long n = __oxbow_fs_pwrite(fds[fd].handle, buf, len, fds[fd].off);
	if (n > 0) {
		fds[fd].off += (unsigned long)n;
		if (fds[fd].off > fds[fd].size)
			fds[fd].size = fds[fd].off;
	}
	return n;
}

/* Fill a Linux x86_64 `struct kstat` (== the kernel stat, 144 bytes). On x86_64
 * musl uses this path, not statx. Offsets per arch/x86_64/kstat.h. */
static void fill_kstat(unsigned char *s, unsigned long size, int kind, unsigned int mtime)
{
	for (int i = 0; i < 144; i++)
		s[i] = 0;
	unsigned int mode = ((kind == 1) ? 0040000u : 0100000u) | 0755u; /* dir or reg */
	*(uint64_t *)(s + 16) = 1;              /* st_nlink   */
	*(uint32_t *)(s + 24) = mode;           /* st_mode    */
	*(int64_t  *)(s + 48) = (int64_t)size;  /* st_size    */
	*(int64_t  *)(s + 56) = 4096;           /* st_blksize */
	*(int64_t  *)(s + 64) = (int64_t)((size + 511) / 512); /* st_blocks */
	*(long *)(s + 72)  = (long)mtime;       /* st_atime_sec */
	*(long *)(s + 88)  = (long)mtime;       /* st_mtime_sec */
	*(long *)(s + 104) = (long)mtime;       /* st_ctime_sec */
}

/* stat a path into a kstat buffer (open-to-stat-then-close). */
static long stat_path(const char *path, unsigned char *kst)
{
	if (!kst)
		return -E_FAULT;
	unsigned long sz = 0;
	int kd = 0;
	unsigned int mt = 0, at = 0;
	long h = __oxbow_fs_open(path, slen(path), 0, &sz, &kd, &mt, &at);
	if (h < 0)
		return -E_NOENT;
	__oxbow_fs_close(h);
	fill_kstat(kst, sz, kd, mt);
	return 0;
}

/* ----------------------- process: fork/exec/wait --------------------------- */
/* Real fork (Phase 3b): the kernel SYS_FORK clones our AS + handle table into a new
 * process; the child resumes at the fork point in its OWN copied AS via the
 * setjmp/longjmp trampoline (see fork_buf above), so there is no shared-stack hazard.
 * execve spawns a program via __oxbow_spawn and runs it to completion (launcher
 * model); waitpid blocks on the child's exit-notif and returns its status. */
#define MAXCHILD 32
static struct child { int used; unsigned int pid; long notif; } children[MAXCHILD];

static void remember_child(unsigned int pid, long notif)
{
	for (int i = 0; i < MAXCHILD; i++)
		if (!children[i].used) {
			children[i].used = 1;
			children[i].pid = pid;
			children[i].notif = notif;
			return;
		}
}
static long child_notif(unsigned int pid)
{
	for (int i = 0; i < MAXCHILD; i++)
		if (children[i].used && children[i].pid == pid)
			return children[i].notif;
	return -1;
}
static void forget_child(unsigned int pid)
{
	for (int i = 0; i < MAXCHILD; i++)
		if (children[i].used && children[i].pid == pid) {
			children[i].used = 0;
			return;
		}
}

/* Read `path`'s ELF and spawn it with argv[1..] joined as the oxbow arg blob,
 * inheriting stdout (slot 2). Returns the new pid (>=0) or -errno. */
static long do_exec_spawn(const char *path, char *const argv[])
{
	static char blob[1024];
	int bl = 0;
	if (argv)
		for (int i = 1; argv[i]; i++) {
			if (bl && bl < 1023)
				blob[bl++] = ' ';
			for (const char *s = argv[i]; *s && bl < 1023; s++)
				blob[bl++] = *s;
		}
	blob[bl < 1024 ? bl : 1023] = 0;

	unsigned long sz = 0;
	int kd = 0;
	unsigned int mt = 0, at = 0;
	long h = __oxbow_fs_open(path, slen(path), 0, &sz, &kd, &mt, &at);
	if (h < 0)
		return -E_NOENT;
	void *elf = malloc(sz ? sz : 1);
	if (!elf) {
		__oxbow_fs_close(h);
		return -E_NOSYS;
	}
	unsigned long got = 0;
	while (got < sz) {
		long r = __oxbow_fs_pread(h, (char *)elf + got, sz - got, got);
		if (r <= 0)
			break;
		got += (unsigned long)r;
	}
	__oxbow_fs_close(h);

	unsigned int pid = 0;
	long notif = __oxbow_spawn(elf, got, blob, (unsigned long)bl, 2 /*SPAWN_STDOUT*/, &pid);
	free(elf);
	if (notif < 0)
		return -E_NOENT;
	remember_child(pid, notif);
	return (long)pid;
}

long __oxbow_syscall(long n, long a1, long a2, long a3, long a4, long a5, long a6)
{
	(void)a6;
	switch (n) {

	/* ---- process ---- */
	case NR_fork:
	case NR_clone: {
		/* Real fork via a kernel AS-clone. setjmp returns 0 here (parent path); the
		 * child resumes via the trampoline's longjmp in its own copied AS and setjmp
		 * returns nonzero. The parent creates the child's exit-notif (so waitpid
		 * works), forks, and remembers pid->notif. */
		if (setjmp(fork_buf) != 0)
			return 0; /* child: fork() returns 0, in its own AS */
		long notif = ox_notif_create(); /* handle is in RDX, not RAX */
		if (notif < 0)
			return -E_NOSYS;
		void *sp = fork_child_stack + sizeof fork_child_stack - 8;
		long pid = ox_syscall3(OX_SYS_FORK, (long)fork_trampoline, (long)sp, notif);
		if (pid > 0)
			remember_child((unsigned int)pid, notif);
		return pid > 0 ? pid : -E_NOSYS;
	}
	case NR_execve: {
		/* Spawn the program, run it to completion, and exit with its status. This
		 * covers the launcher / "exec as the last thing" case on a spawn kernel. */
		long pid = do_exec_spawn((const char *)a1, (char *const *)a2);
		if (pid < 0)
			return pid; /* exec failed; caller typically _exit(127) */
		long nf = child_notif((unsigned int)pid);
		int code = __oxbow_wait(nf);
		forget_child((unsigned int)pid);
		ox_syscall1(OX_SYS_EXIT, code);
		__builtin_unreachable();
	}
	case NR_wait4: {
		unsigned int pid = (unsigned int)a1;
		int *status = (int *)a2;
		long nf = child_notif(pid);
		if (nf < 0)
			return -E_NOSYS; /* no such child */
		int code = __oxbow_wait(nf);
		forget_child(pid);
		if (status)
			*status = (code & 0xff) << 8; /* WIFEXITED|WEXITSTATUS encoding */
		return (long)pid;
	}

	/* ---- I/O on fds ---- */
	case NR_read:
		return do_read(a1, (void *)a2, (unsigned long)a3);
	case NR_write:
		return do_write(a1, (const void *)a2, (unsigned long)a3);
	case NR_readv: {
		const struct { void *base; unsigned long len; } *iov = (const void *)a2;
		long total = 0;
		for (long i = 0; i < a3; i++) {
			if (!iov[i].len)
				continue;
			long r = do_read(a1, iov[i].base, iov[i].len);
			if (r < 0)
				return total ? total : r;
			total += r;
			if ((unsigned long)r < iov[i].len)
				break;
		}
		return total;
	}
	case NR_writev: {
		const struct { const void *base; unsigned long len; } *iov = (const void *)a2;
		long total = 0;
		for (long i = 0; i < a3; i++) {
			if (!iov[i].len)
				continue;
			long w = do_write(a1, iov[i].base, iov[i].len);
			if (w < 0)
				return total ? total : w;
			total += w;
		}
		return total;
	}

	/* ---- files ---- */
	case NR_open:
		return do_open((const char *)a1, a2);
	case NR_openat:
		/* a1=dirfd (only AT_FDCWD / absolute supported now), a2=path, a3=flags */
		return do_open((const char *)a2, a3);
	case NR_close:
		if (a1 < 3)
			return 0;
		if (a1 >= MAXFD || !fds[a1].used)
			return -E_BADF;
		__oxbow_fs_close(fds[a1].handle);
		fds[a1].used = 0;
		return 0;
	case NR_lseek: {
		long fd = a1, off = a2;
		int whence = (int)a3;
		if (fd < 3 || fd >= MAXFD || !fds[fd].used)
			return -E_BADF;
		unsigned long base = (whence == 0) ? 0
		                   : (whence == 1) ? fds[fd].off
		                                   : fds[fd].size; /* SEEK_SET/CUR/END */
		long no = (long)base + off;
		if (no < 0)
			return -E_INVAL;
		fds[fd].off = (unsigned long)no;
		return no;
	}
	/* stat family — on x86_64 musl uses the kstat path (not statx). */
	case NR_stat:  /* a1=path, a2=kstat */
	case NR_lstat:
		return stat_path((const char *)a1, (unsigned char *)a2);
	case NR_fstat: { /* a1=fd, a2=kstat */
		long fd = a1;
		unsigned char *kst = (unsigned char *)a2;
		if (!kst)
			return -E_FAULT;
		unsigned long sz = 0;
		if (fd >= 3 && fd < MAXFD && fds[fd].used)
			sz = fds[fd].size;
		fill_kstat(kst, sz, 2, 0);
		return 0;
	}
	case NR_newfstatat: { /* a1=dirfd, a2=path, a3=kstat, a4=flag */
		const char *path = (const char *)a2;
		unsigned char *kst = (unsigned char *)a3;
		int flag = (int)a4;
		if (!kst)
			return -E_FAULT;
		if ((flag & AT_EMPTY_PATH) || !path || path[0] == 0) {
			long fd = a1;
			unsigned long sz = 0;
			if (fd >= 3 && fd < MAXFD && fds[fd].used)
				sz = fds[fd].size;
			fill_kstat(kst, sz, 2, 0);
			return 0;
		}
		return stat_path(path, kst);
	}
	case NR_statx: { /* not used on x86_64, but handle it for completeness */
		const char *path = (const char *)a2;
		unsigned char *kst = (unsigned char *)a5; /* note: wrong layout, but unreached */
		(void)path;
		(void)kst;
		return -E_NOSYS;
	}
	case NR_getcwd: {
		char *buf = (char *)a1;
		unsigned long size = (unsigned long)a2;
		if (!buf || size < 2)
			return -E_INVAL;
		buf[0] = '/';
		buf[1] = 0;
		return 2; /* length including NUL */
	}

	/* ---- memory ---- */
	case NR_mmap:
		/* __oxbow_mmap_anon always maps RW (ignoring PROT_NONE); musl's mallocng
		 * mmaps PROT_NONE then mprotects to RW, which we make a no-op below. */
		return (long)__oxbow_mmap_anon((unsigned long)a2);
	case NR_munmap:
		return 0;
	case NR_mprotect:
		/* Our anonymous mappings are already RW, so making them RW is a no-op
		 * success. (W^X downgrades to NONE/RO are not yet enforced — Phase 4+.) */
		return 0;
	case NR_madvise:
		return 0;
	case NR_brk:
		/* No brk: report a fixed break that never grows, so mallocng's
		 * `brk(new) != new` check trips and it cleanly falls back to mmap. */
		return 0x10000000;

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
		int op = (int)a2 & 0x7f;
		if (op == 0)
			return ox_syscall3(OX_SYS_FUTEX_WAIT, a1, a3, 0);
		if (op == 1)
			return ox_syscall2(OX_SYS_FUTEX_WAKE, a1, a3);
		return -E_INVAL;
	}

	/* ---- terminal (termios) on the std streams ---- */
	case NR_ioctl: {
		long fd = a1;
		unsigned long req = (unsigned long)a2;
		void *arg = (void *)a3;
		if (fd < 0 || fd > 2) /* only stdin/out/err are ttys for now */
			return -E_NOTTY;
		switch (req) {
		case TIOCGWINSZ: {
			/* {ws_row, ws_col, ws_xpixel, ws_ypixel}. A nonzero size + success is
			 * what makes isatty() true (musl's isatty issues TIOCGWINSZ). */
			unsigned short *ws = (unsigned short *)arg;
			if (ws) {
				ws[0] = 24;
				ws[1] = 80;
				ws[2] = 0;
				ws[3] = 0;
			}
			return 0;
		}
		case TCGETS: {
			/* struct termios (44 bytes): cooked defaults so tcgetattr reports a
			 * line-disciplined terminal (ICANON|ECHO|ISIG). */
			unsigned char *t = (unsigned char *)arg;
			if (t) {
				for (int i = 0; i < 44; i++)
					t[i] = 0;
				*(uint32_t *)(t + 0)  = 0x0500; /* c_iflag: ICRNL|IXON */
				*(uint32_t *)(t + 4)  = 0x0005; /* c_oflag: OPOST|ONLCR */
				*(uint32_t *)(t + 8)  = 0x00bf; /* c_cflag: B38400|CS8|CREAD */
				*(uint32_t *)(t + 12) = T_ISIG | T_ICANON | T_ECHO | T_ECHOE | T_ECHOK | T_IEXTEN;
				t[17 + 0] = 3;    /* VINTR  = ^C   */
				t[17 + 1] = 0x1c; /* VQUIT  = ^\   */
				t[17 + 2] = 0x7f; /* VERASE = DEL  */
				t[17 + 3] = 0x15; /* VKILL  = ^U   */
				t[17 + 4] = 4;    /* VEOF   = ^D   */
				t[17 + 6] = 1;    /* VMIN   = 1    */
			}
			return 0;
		}
		case TCSETS:
		case TCSETSW:
		case TCSETSF:
			/* Accept attribute changes (e.g. a REPL switching to raw mode). The
			 * tty input path honoring raw mode is a later step; succeeding here
			 * lets the program run rather than erroring out. */
			return 0;
		default:
			return -E_NOTTY;
		}
	}

	/* ---- signals: accept installs as no-ops ---- */
	case NR_rt_sigaction:
	case NR_rt_sigprocmask:
		return 0;

	/* ---- single-user identity (root) ---- */
	case NR_getuid:
	case NR_geteuid:
	case NR_getgid:
	case NR_getegid:
		return 0;

	/* ---- exit ---- */
	case NR_exit:
	case NR_exit_group:
		ox_syscall1(OX_SYS_EXIT, a1);
		__builtin_unreachable();

	/* ---- not yet: fork/exec (Phase 3), the rest ---- */
	case NR_dup:
	case NR_dup2:
	case NR_fcntl:
	case NR_access:
	case NR_readlink:
	case NR_uname:
	case NR_nanosleep:
	default:
		return -E_NOSYS;
	}
}
