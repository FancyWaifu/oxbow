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
/* fd kinds. FILE backs a fsd file (handle = fsd cap, uses off/size); PIPE_R/PIPE_W
 * back an oxbow pipe endpoint (handle = pipe handle); TTY is the interactive console
 * (fds 0/1/2 by default — not table-resident unless dup2 redirects them). */
#define K_FILE   0
#define K_PIPE_R 1
#define K_PIPE_W 2
#define K_DIR    3  /* an open directory; .off is the readdir cursor */
#define K_TTY    4  /* a dup of a std tty stream (.handle = 0/1/2); shells dup these */
struct oxfd {
	int used;
	int kind;
	long handle;         /* fsd file cap (FILE) or pipe handle (PIPE_*) */
	unsigned long off;   /* current file offset (FILE only) */
	unsigned long size;  /* known size, grows on write (FILE only) */
};
static struct oxfd fds[MAXFD];

static unsigned long slen(const char *s)
{
	unsigned long n = 0;
	while (s && s[n])
		n++;
	return n;
}

static int fd_alloc_kind(long handle, unsigned long size, int kind)
{
	for (int i = 3; i < MAXFD; i++) {
		if (!fds[i].used) {
			fds[i].used = 1;
			fds[i].kind = kind;
			fds[i].handle = handle;
			fds[i].off = 0;
			fds[i].size = size;
			return i;
		}
	}
	return -1;
}

static int fd_alloc(long handle, unsigned long size)
{
	return fd_alloc_kind(handle, size, K_FILE);
}

/* Close whatever an fd backs (pipe endpoints need an explicit pipe close). */
static void fd_release(int fd)
{
	if (fd < 0 || fd >= MAXFD || !fds[fd].used)
		return;
	if (fds[fd].kind == K_PIPE_W) {
		/* Closing a write end signals EOF to readers (oxbow pipes don't refcount
		 * writers, so the holder must do this — mirrors the shell's pipeline). */
		__oxbow_pipe_eof((unsigned int)fds[fd].handle);
		__oxbow_pipe_close((unsigned int)fds[fd].handle);
	} else if (fds[fd].kind == K_PIPE_R) {
		__oxbow_pipe_close((unsigned int)fds[fd].handle);
	} else if (fds[fd].kind == K_FILE || fds[fd].kind == K_DIR) {
		/* dup2/F_DUPFD share the fsd handle, so only release it when NO other fd
		 * still references it (shells dup a fd then close the original — closing the
		 * shared handle would kill the dup). */
		int shared = 0;
		for (int i = 0; i < MAXFD; i++)
			if (i != fd && fds[i].used &&
			    (fds[i].kind == K_FILE || fds[i].kind == K_DIR) &&
			    fds[i].handle == fds[fd].handle) {
				shared = 1;
				break;
			}
		if (!shared)
			__oxbow_fs_close(fds[fd].handle);
	}
	fds[fd].used = 0;
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
	int fd = fd_alloc_kind(h, size, (kind == 1) ? K_DIR : K_FILE);
	if (fd < 0) {
		__oxbow_fs_close(h);
		return -E_MFILE;
	}
	return fd;
}

static long deliver_self(int sig); /* forward: SIGINT on a Ctrl-C'd tty read */

/* read/write dispatch on the fd's kind. A table-resident fd (incl. one dup2'd onto
 * 0/1/2) uses its kind; a bare 0/1/2 is the interactive tty. */
static long do_read(long fd, void *buf, unsigned long len)
{
	if (fd >= 0 && fd < MAXFD && fds[fd].used) {
		if (fds[fd].kind == K_PIPE_R)
			return __oxbow_pipe_read((unsigned int)fds[fd].handle, buf, len);
		if (fds[fd].kind == K_FILE) {
			long n = __oxbow_fs_pread(fds[fd].handle, buf, len, fds[fd].off);
			if (n > 0)
				fds[fd].off += (unsigned long)n;
			return n;
		}
		if (fds[fd].kind == K_TTY)
			return do_read(fds[fd].handle, buf, len); /* route to the std stream */
		return -E_BADF; /* read on a write-only pipe end */
	}
	if (fd < 3) {
		long n = __oxbow_read((int)fd, buf, len);
		if (n == OX_READ_EINTR) {  /* Ctrl-C while blocked for input */
			deliver_self(2);   /* SIGINT: run handler, or default-terminate */
			return -E_INTR;    /* a handler ran -> report EINTR to the caller */
		}
		return n;
	}
	return -E_BADF;
}

static long do_write(long fd, const void *buf, unsigned long len)
{
	if (fd >= 0 && fd < MAXFD && fds[fd].used) {
		if (fds[fd].kind == K_PIPE_W)
			return __oxbow_pipe_write((unsigned int)fds[fd].handle, buf, len);
		if (fds[fd].kind == K_FILE) {
			long n = __oxbow_fs_pwrite(fds[fd].handle, buf, len, fds[fd].off);
			if (n > 0) {
				fds[fd].off += (unsigned long)n;
				if (fds[fd].off > fds[fd].size)
					fds[fd].size = fds[fd].off;
			}
			return n;
		}
		if (fds[fd].kind == K_TTY)
			return do_write(fds[fd].handle, buf, len); /* route to the std stream */
		return -E_BADF; /* write on a read-only pipe end */
	}
	if (fd == 1 || fd == 2)
		return __oxbow_write((int)fd, buf, len);
	return -E_BADF;
}

/* pipe(): create an oxbow pipe pair, install its ends in the fd table, write the two
 * fds to fd_out[0]=read, fd_out[1]=write. Backs pipe()/pipe2() and (with fork) the
 * popen()/system() machinery. */
static long do_pipe(int *fd_out)
{
	unsigned int re = 0, we = 0;
	if (__oxbow_pipe(&re, &we) != 0)
		return -E_INVAL;
	int rfd = fd_alloc_kind((long)re, 0, K_PIPE_R);
	int wfd = fd_alloc_kind((long)we, 0, K_PIPE_W);
	if (rfd < 0 || wfd < 0) {
		if (rfd >= 0)
			fd_release(rfd);
		else
			__oxbow_pipe_close(re);
		if (wfd >= 0)
			fd_release(wfd);
		else
			__oxbow_pipe_close(we);
		return -E_MFILE;
	}
	fd_out[0] = rfd;
	fd_out[1] = wfd;
	return 0;
}

/* dup2(old,new): make `new` refer to whatever `old` does. Pipe ends are dup'd via
 * the pipe handle (a fresh refcount); file fds share the fsd cap (close-once caveat,
 * acceptable for the redirect-then-exec idiom). Returns new. */
static long do_dup2(long oldfd, long newfd)
{
	if (oldfd < 0 || oldfd >= MAXFD || !fds[oldfd].used)
		return -E_BADF;
	if (newfd < 0 || newfd >= MAXFD)
		return -E_BADF;
	if (oldfd == newfd)
		return newfd;
	fd_release((int)newfd);
	fds[newfd] = fds[oldfd];
	fds[newfd].used = 1;
	if (fds[oldfd].kind == K_PIPE_R || fds[oldfd].kind == K_PIPE_W) {
		long nh = __oxbow_pipe_dup((unsigned int)fds[oldfd].handle);
		fds[newfd].handle = (nh >= 0) ? nh : fds[oldfd].handle;
	}
	return newfd;
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

	/* Honor a dup2'd stdout: if fd 1 was redirected onto a pipe (the popen /
	 * `awk | cmd` idiom: pipe()+fork()+dup2(we,1)+exec), hand the exec'd child that
	 * pipe as its stdout instead of our tty. Otherwise inherit our SPAWN_STDOUT(=2). */
	unsigned int stdout_cap = 2;
	if (fds[1].used && fds[1].kind == K_PIPE_W)
		stdout_cap = (unsigned int)fds[1].handle;
	/* Honor a dup2'd stdin too: popen("w") / a pipeline does dup2(pipe_r, 0) +
	 * exec, so the child reads its stdin from the pipe. 0 = inherit ours. */
	unsigned int stdin_cap = 0;
	if (fds[0].used && fds[0].kind == K_PIPE_R)
		stdin_cap = (unsigned int)fds[0].handle;

	unsigned int pid = 0;
	long notif = __oxbow_spawn(elf, got, blob, (unsigned long)bl, stdout_cap, stdin_cap, &pid);
	free(elf);
	if (notif < 0)
		return -E_NOENT;
	remember_child(pid, notif);
	return (long)pid;
}

/* ----------------------------- signals ------------------------------------- */
/* Per-process signal state: sigaction records handlers + the blocked mask, and a
 * self-directed tkill/tgkill/kill (raise/abort) delivers SYNCHRONOUSLY — it runs the
 * installed handler, or applies the default action (terminate, or ignore for a few).
 * External/async delivery (SIGINT from the tty, signals from another process) needs
 * a kernel signal-frame mechanism + a sigreturn — a later step. */
#define NSIG 65
static struct ksigaction { void (*handler)(int); unsigned long flags; void (*restorer)(void); unsigned long mask; } sigtab[NSIG];
static unsigned long sig_blocked;
static unsigned long sig_pending;

/* Signals whose default action is "ignore" — SIGCHLD/SIGCONT/SIGURG/SIGWINCH. */
static int sig_default_ignores(int sig)
{
	return sig == 17 || sig == 18 || sig == 23 || sig == 28;
}

/* Run `sig`'s action NOW (handler, or the default) — caller has checked it's not
 * blocked. */
static void run_sig(int sig)
{
	void (*h)(int) = sigtab[sig].handler;
	if (h == (void *)0) { /* SIG_DFL */
		if (sig_default_ignores(sig))
			return;
		ox_syscall1(OX_SYS_EXIT, 128 + sig); /* default: terminate */
		__builtin_unreachable();
	}
	if (h == (void *)1) /* SIG_IGN */
		return;
	h(sig); /* run the handler synchronously (self-raise / abort) */
}

/* Deliver `sig` to ourselves: if blocked, latch it pending (musl's raise() blocks
 * around the tkill, then unblocks — that's when it must fire); else run it now. */
static long deliver_self(int sig)
{
	if (sig < 1 || sig >= NSIG)
		return -E_INVAL;
	if (sig_blocked & (1UL << (sig - 1))) {
		sig_pending |= (1UL << (sig - 1));
		return 0;
	}
	run_sig(sig);
	return 0;
}

/* After a mask change, run any now-unblocked pending signals (the delivery point
 * for a signal raised while blocked). */
static void deliver_pending(void)
{
	unsigned long ready = sig_pending & ~sig_blocked;
	for (int sig = 1; sig < NSIG; sig++) {
		if (ready & (1UL << (sig - 1))) {
			sig_pending &= ~(1UL << (sig - 1));
			run_sig(sig);
		}
	}
}

/* ---- async signal dispatch (Phase 9 step 2) ----
 * The kernel injects a frame that enters __oxbow_sig_dispatch with rdi=signum and
 * rsp pointing at a saved context (the interrupted [rip,rflags,rsp,rax]); we run the
 * handler, then SYS_SIGRETURN to restore. Called for SIGINT from a running program
 * (Ctrl-C while not at a read boundary). */
void __oxbow_run_signal(int sig)
{
	run_sig(sig); /* the installed handler, or the default action (terminate) */
}

/* The trampoline the kernel redirects to. rsp = the sigcontext pointer on entry
 * (16-aligned); rbx (callee-saved) preserves it across the C handler call, then it's
 * handed to SYS_SIGRETURN, which never returns here. */
__asm__(".text\n"
	".globl __oxbow_sig_dispatch\n"
	"__oxbow_sig_dispatch:\n"
	"	movq %rsp, %rbx\n"
	"	call __oxbow_run_signal\n"
	"	movq %rbx, %rdi\n"
	"	movl $68, %eax\n" /* OX_SYS_SIGRETURN */
	"	syscall\n"
	"	ud2\n");

/* Register our async-signal dispatcher with the kernel (call once at startup). */
void __oxbow_register_sigdispatch(void)
{
	extern void __oxbow_sig_dispatch(void);
	ox_syscall1(OX_SYS_SIGDISPATCH, (long)&__oxbow_sig_dispatch);
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
		if (a1 < 0 || a1 >= MAXFD)
			return -E_BADF;
		if (!fds[a1].used)
			return (a1 < 3) ? 0 : -E_BADF; /* bare std streams: nothing to close */
		fd_release((int)a1); /* pipe ends + files freed by kind */
		return 0;
	case NR_ftruncate: {
		/* ftruncate(fd, len): set a file's length. Editors (kilo) rewrite a file as
		 * ftruncate(len) + write(len); without this, the dirty flag never clears. */
		long fd = a1;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used || fds[fd].kind != K_FILE)
			return -E_BADF;
		if (__oxbow_fs_truncate(fds[fd].handle, (unsigned long)a2) != 0)
			return -E_INVAL;
		fds[fd].size = (unsigned long)a2;
		if (fds[fd].off > fds[fd].size)
			fds[fd].off = fds[fd].size;
		return 0;
	}
	case NR_getdents64: {
		/* Fill `buf` with linux_dirent64 records by walking the dir via fsd's readdir
		 * (fds[fd].off is the cursor). Backs opendir()/readdir() — needed by shells,
		 * find, ls, ./configure. Returns bytes written, 0 at end of directory. */
		long fd = a1;
		unsigned char *buf = (unsigned char *)a2;
		unsigned long cap = (unsigned long)a3;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used || fds[fd].kind != K_DIR)
			return -E_BADF;
		if (!buf)
			return -E_FAULT;
		unsigned long off = 0;
		char nm[256];
		unsigned int dkind = 0;
		for (;;) {
			long nl = __oxbow_fs_readdir(fds[fd].handle, fds[fd].off,
						     (unsigned char *)nm, sizeof nm - 1, &dkind);
			if (nl < 0)
				break; /* end of directory */
			if (nl > 255)
				nl = 255;
			/* linux_dirent64: d_ino(8) d_off(8) d_reclen(2) d_type(1) d_name[],
			 * the whole record padded to an 8-byte multiple. */
			unsigned long reclen = (19 + (unsigned long)nl + 1 + 7) & ~7UL;
			if (off + reclen > cap) {
				if (off == 0)
					return -E_INVAL; /* buffer too small for one entry */
				break;               /* full; this entry comes next call (cursor unmoved) */
			}
			unsigned char *e = buf + off;
			*(uint64_t *)(e + 0) = (uint64_t)(fds[fd].off + 1); /* d_ino (nonzero) */
			*(int64_t *)(e + 8) = (int64_t)(fds[fd].off + 1);   /* d_off */
			*(uint16_t *)(e + 16) = (uint16_t)reclen;
			e[18] = (dkind == 1) ? 4 : 8; /* d_type: DT_DIR / DT_REG */
			for (long i = 0; i < nl; i++)
				e[19 + i] = (unsigned char)nm[i];
			e[19 + nl] = 0;
			off += reclen;
			fds[fd].off++; /* advance the readdir cursor */
		}
		return (long)off;
	}
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
		int kd = 2; /* default: regular file */
		if (fd >= 3 && fd < MAXFD && fds[fd].used) {
			sz = fds[fd].size;
			if (fds[fd].kind == K_DIR)
				kd = 1; /* directory — opendir()/fstat() must see S_IFDIR */
		}
		fill_kstat(kst, sz, kd, 0);
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
	case NR_getppid:
		return 1; /* oxbow doesn't expose the parent pid; a stable value suffices */

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
			/* Switch the tty line discipline by the ICANON bit: a TUI app (editor)
			 * clears ICANON for raw keystroke input; a shell/REPL keeps it. Only the
			 * std tty streams drive the console mode (a redirected/piped fd doesn't). */
			if (a1 >= 0 && a1 < 3 && !(fds[a1].used && fds[a1].kind != K_FILE)) {
				const unsigned char *t = (const unsigned char *)arg;
				unsigned int lflag = t ? *(const uint32_t *)(t + 12) : T_ICANON;
				__oxbow_tty_mode((lflag & T_ICANON) ? 0 : 1);
			}
			return 0;
		default:
			return -E_NOTTY;
		}
	}

	/* ---- signals ---- */
	case NR_rt_sigaction: {
		int sig = (int)a1;
		struct ksigaction *na = (struct ksigaction *)a2;
		struct ksigaction *oa = (struct ksigaction *)a3;
		if (sig < 1 || sig >= NSIG)
			return -E_INVAL;
		if (oa) {
			oa->handler = sigtab[sig].handler;
			oa->flags = sigtab[sig].flags;
			oa->restorer = 0;
			oa->mask = 0;
		}
		if (na) {
			sigtab[sig].handler = na->handler;
			sigtab[sig].flags = na->flags;
		}
		return 0;
	}
	case NR_rt_sigprocmask: {
		int how = (int)a1;
		unsigned long *set = (unsigned long *)a2;
		unsigned long *old = (unsigned long *)a3;
		if (old)
			*old = sig_blocked;
		if (set) {
			if (how == 0)
				sig_blocked |= *set; /* SIG_BLOCK */
			else if (how == 1)
				sig_blocked &= ~*set; /* SIG_UNBLOCK */
			else if (how == 2)
				sig_blocked = *set; /* SIG_SETMASK */
			deliver_pending(); /* fire anything that just became unblocked */
		}
		return 0;
	}
	case NR_tkill: /* raise(): (tid, sig) */
		return deliver_self((int)a2);
	case NR_tgkill: /* (tgid, tid, sig) */
		return deliver_self((int)a3);
	case NR_kill: /* (pid, sig) — self-delivery only for now */
		return deliver_self((int)a2);

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

	/* ---- I/O multiplexing (pragmatic: report the requested fds ready, so a
	 * poll/select-then-read works — reads block for real data) ---- */
	case NR_poll:
	case NR_ppoll: {
		struct pfd {
			int fd;
			short events;
			short revents;
		} *pf = (struct pfd *)a1;
		unsigned long nfds = (unsigned long)a2;
		int ready = 0;
		for (unsigned long i = 0; i < nfds && pf; i++) {
			short re = (pf[i].fd >= 0) ? (short)(pf[i].events & 0x7) : 0; /* IN|PRI|OUT */
			pf[i].revents = re;
			if (re)
				ready++;
		}
		return ready;
	}
	case NR_select:
	case NR_pselect6: {
		/* nfds=a1; readfds=a2, writefds=a3 (fd_set bitmaps). Leave the sets as-is
		 * (all requested fds reported ready) + clear exceptfds; return the count. */
		int nfds = (int)a1;
		unsigned long *rd = (unsigned long *)a2;
		unsigned long *wr = (unsigned long *)a3;
		unsigned long *ex = (unsigned long *)a4;
		int ready = 0;
		for (int fd = 0; fd < nfds && fd < 1024; fd++) {
			unsigned long bit = 1UL << (fd & 63);
			int w = fd >> 6;
			if (rd && (rd[w] & bit))
				ready++;
			if (wr && (wr[w] & bit))
				ready++;
		}
		if (ex)
			for (int w = 0; w < (nfds + 63) / 64 && w < 16; w++)
				ex[w] = 0;
		return ready;
	}

	/* ---- pipes + fd duplication (Phase 6) ---- */
	case NR_pipe:
		return do_pipe((int *)a1);
	case NR_pipe2: /* flags (O_CLOEXEC/O_NONBLOCK) ignored — no exec-close yet */
		return do_pipe((int *)a1);
	case NR_dup2:
		return do_dup2(a1, a2);
	case NR_dup3: /* flags ignored */
		return do_dup2(a1, a2);
	case NR_dup: {
		for (int i = 3; i < MAXFD; i++)
			if (!fds[i].used)
				return do_dup2(a1, i);
		return -E_MFILE;
	}
	case NR_fcntl: {
		/* Shells (dash) rely on this for fd juggling around redirections. */
		long fd = a1;
		unsigned long cmd = (unsigned long)a2;
		long arg = a3;
		switch (cmd) {
		case F_DUPFD:
		case F_DUPFD_CLOEXEC: {
			/* Lowest free fd >= arg referring to the same object. CLOEXEC isn't
			 * enforced (we have no exec-time fd close), which is harmless here. */
			int start = (arg < 3) ? 3 : (int)arg;
			for (int i = start; i < MAXFD; i++) {
				if (fds[i].used)
					continue;
				if (fd >= 0 && fd < 3 && !fds[fd].used) {
					/* dup of a bare std tty stream -> a K_TTY alias */
					fds[i].used = 1;
					fds[i].kind = K_TTY;
					fds[i].handle = fd;
					fds[i].off = 0;
					fds[i].size = 0;
					return i;
				}
				return do_dup2(fd, i);
			}
			return -E_MFILE;
		}
		case F_GETFD:
			return 0; /* no FD_CLOEXEC tracked */
		case F_SETFD:
			return 0; /* accept (CLOEXEC not enforced) */
		case F_GETFL:
			return 2; /* O_RDWR */
		case F_SETFL:
			return 0; /* accept O_NONBLOCK/etc. */
		default:
			return 0; /* accept other fcntls benignly */
		}
	}

	/* ---- not yet: the rest ---- */
	case NR_readlink:
	case NR_uname:
	default:
		return -E_NOSYS;
	}
}
