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

/* memcpy is provided by musl libc.a at link; declared here since this freestanding
 * dispatcher doesn't pull <string.h>. Used by the §wayland send/recvmsg iov copies. */
extern void *memcpy(void *, const void *, unsigned long);

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
		unsigned long ms = ox_uptime_ms();
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
#define K_SOCK   5  /* a TCP socket; .handle = socket cap (-1 until connect succeeds) */
#define K_UDP    6  /* a UDP socket; .handle = socket cap (-1 until bind/first send) */
#define K_LISTEN 7  /* a TCP listener; .handle = listener cap (accept mints K_SOCK fds) */
#define K_CHAN   8  /* §wayland: a channel cap (the Wayland display socket); .handle = chan cap */
#define K_SHM    9  /* §wayland: an shm/memfd region; .handle = shm cap (-1 until ftruncate), .size = bytes */
#define K_PTYM   10 /* §pty: pty MASTER (.handle = master cap, .off = pts number) */
#define K_PTYS   11 /* §pty: pty SLAVE  (.handle = slave cap) — the shell's controlling tty */
#define K_EPOLL  12 /* §weston: an epoll set; registrations live in g_epoll[] keyed by .handle */
#define K_TIMERFD 13 /* §weston: a timerfd; .off = absolute deadline ms (0 = disarmed), .size = interval ms */
#define K_SIGNALFD 14 /* §weston: a signalfd — stubbed (never readable; signals don't fire) */
#define K_EVENTFD  15 /* §weston: an eventfd counter; .off = current count */
struct oxfd {
	int used;
	int kind;
	long handle;         /* fsd file cap (FILE) or pipe handle (PIPE_*) */
	unsigned long off;   /* current file offset (FILE only) */
	unsigned long size;  /* known size, grows on write (FILE only) */
	int nonblock;        /* O_NONBLOCK (K_SOCK): read() returns EAGAIN instead of blocking */
	int owns;            /* socket fds: 1 = close() destroys the net-server socket; 0 = a
	                      * forked child's BORROWED copy (close drops the cap only). */
	unsigned int seals;  /* §wayland: memfd F_ADD_SEALS bits (F_GET_SEALS reports them). A
	                      * read-only-sealed keymap memfd is NOT closed by weston's put_fd,
	                      * so the fd survives until libwayland flushes it to the client. */
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
			fds[i].nonblock = 0;
			fds[i].seals = 0;
			fds[i].owns = 1; /* a freshly created fd owns its handle (cleared in fork child) */
			return i;
		}
	}
	return -1;
}

static int fd_alloc(long handle, unsigned long size)
{
	return fd_alloc_kind(handle, size, K_FILE);
}

/* §wayland: vaddr bump for shm maps. Sits ABOVE oxbow-rt's anon mmap window
 * [0x4000_0000, 0x6000_0000) so the two never collide. */
static unsigned long g_shm_next = 0x60000000UL;

/* §41 shm lifetime: an mmap of a wl_shm region keeps that region alive as long as the
 * MAPPING exists — which outlives the memfd fd (wl_shm closes the fd right after mmap,
 * but keeps rendering into the mapping until the pixmap is destroyed). So we hold the
 * shm cap here, keyed by the mapped vaddr, and drop it (SYS_CLOSE → kernel decref →
 * free when the compositor also drops its granted copy) at munmap, NOT at close(fd).
 * Without this every X pixmap leaked a kernel shm region → the 16-slot pool exhausted
 * and Xwayland died. 64 covers plenty of concurrent wl_shm buffers. */
static struct { unsigned long va; long handle; } g_shm_maps[64];
static int shm_map_track(unsigned long va, long handle)
{
	for (int i = 0; i < 64; i++)
		if (g_shm_maps[i].handle == 0 && g_shm_maps[i].va == 0) {
			g_shm_maps[i].va = va;
			g_shm_maps[i].handle = handle + 1; /* +1 so handle 0 isn't "empty" */
			return 1;
		}
	return 0; /* table full: the region will leak, but we don't corrupt state */
}
/* Is `handle` still owned by a live mapping? (so close(fd) must NOT drop the cap) */
static int shm_handle_mapped(long handle)
{
	for (int i = 0; i < 64; i++)
		if (g_shm_maps[i].handle == handle + 1)
			return 1;
	return 0;
}
/* Drop the mapping tracked at `va`; returns the shm cap to close, or -1 if untracked. */
static long shm_map_untrack(unsigned long va)
{
	for (int i = 0; i < 64; i++)
		if (g_shm_maps[i].handle != 0 && g_shm_maps[i].va == va) {
			long h = g_shm_maps[i].handle - 1;
			g_shm_maps[i].va = 0;
			g_shm_maps[i].handle = 0;
			return h;
		}
	return -1;
}

/* §pty: bridge open("/dev/ptmx") → open("/dev/pts/N"). When ptmx is opened we create
 * the pty (master+slave caps) and stash (pts number → slave cap) here; the matching
 * /dev/pts/N open consumes it. Small table covers a few concurrent openpty()s. */
static struct { int n; long slave; } g_pts[4] = { { -1, -1 }, { -1, -1 }, { -1, -1 }, { -1, -1 } };
static int g_pts_counter = 0;

/* §wayland: wrap an inherited capability slot (the Wayland display channel oxcomp
 * hands the client at spawn) as a stream fd. A spawn-slot handle IS the small integer
 * `slot`. The (patched) havoc calls wl_display_connect_to_fd(ox_chan_fd(SLOT)). */
int ox_chan_fd(int slot)
{
	return fd_alloc_kind(slot, 0, K_CHAN);
}

/* Close whatever an fd backs (pipe endpoints need an explicit pipe close). */
static void fd_release(int fd)
{
	if (fd < 0 || fd >= MAXFD || !fds[fd].used)
		return;
	if (fds[fd].kind == K_PIPE_R || fds[fd].kind == K_PIPE_W) {
		/* Just close the handle. The KERNEL marks the pipe EOF when its LAST write
		 * end is dropped (§Phase 11 writer-refcount) — so a fork+exec child closing
		 * its copy of a write end no longer EOFs the pipe out from under siblings
		 * (which broke fork-based pipelines + command substitution). */
		__oxbow_pipe_close((unsigned int)fds[fd].handle);
	} else if (fds[fd].kind == K_SOCK || fds[fd].kind == K_LISTEN) {
		if (fds[fd].handle >= 0) {
			if (fds[fd].owns)
				__oxbow_sock_close(fds[fd].handle); /* FIN / drop the socket|listener cap */
			else
				__oxbow_sock_release(fds[fd].handle); /* borrowed (fork child): cap only */
		}
	} else if (fds[fd].kind == K_UDP) {
		if (fds[fd].handle >= 0) {
			if (fds[fd].owns)
				__oxbow_sock_udp_close(fds[fd].handle);
			else
				__oxbow_sock_release(fds[fd].handle);
		}
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
	} else if (fds[fd].kind == K_SHM) {
		/* §41: a wl_shm memfd. If a live mapping still owns this region's cap, leave it
		 * — munmap drops it. Otherwise (created/ftruncated but the mapping is already
		 * gone, or never mapped) drop the cap now so the region can be reclaimed. */
		if (fds[fd].handle >= 0 && !shm_handle_mapped(fds[fd].handle))
			ox_syscall1(OX_SYS_CLOSE, fds[fd].handle);
	}
	fds[fd].used = 0;
}

/* Tiny strcmp/atoi (no <string.h> in this freestanding dispatcher). */
static int peq(const char *a, const char *b)
{
	while (*a && *a == *b) { a++; b++; }
	return *a == *b;
}

static long do_open(const char *path, long flags)
{
	if (!path)
		return -E_FAULT;

	/* §pty: open("/dev/tty") is the process's CONTROLLING terminal. A GUI app spawned by
	 * the compositor (e.g. xterm) has none, so report ENXIO ("no controlling terminal") —
	 * apps treat that as "use default tty modes / no job control" rather than a hard error
	 * (xterm's ERROR_OPDEVTTY only fires on an unexpected errno). */
	if (peq(path, "/dev/tty"))
		return -6; /* ENXIO */

	/* §pty: open("/dev/ptmx") allocates a pty — the master fd here, the slave stashed
	 * for the matching open("/dev/pts/N"). Makes musl's openpty()/forkpty() work. */
	if (peq(path, "/dev/ptmx")) {
		unsigned int slave = 0;
		long master = ox_pty_create(&slave);
		if (master < 0)
			return -12; /* ENOMEM */
		int slot = -1;
		for (int i = 0; i < 4; i++)
			if (g_pts[i].n < 0) { slot = i; break; }
		if (slot < 0)
			return -E_MFILE;
		int n = g_pts_counter++;
		g_pts[slot].n = n;
		g_pts[slot].slave = (long)slave;
		int fd = fd_alloc_kind(master, 0, K_PTYM);
		if (fd < 0)
			return -E_MFILE;
		fds[fd].off = (unsigned long)n; /* the pts number, for TIOCGPTN */
		return fd;
	}
	/* §shm: POSIX shared memory — musl's shm_open(name) opens "/dev/shm/name". Back it
	 * with an oxbow shm region, exactly like memfd_create (ftruncate allocates it, mmap
	 * maps it, and wl_shm_create_pool passes the fd to the compositor via SCM_RIGHTS).
	 * havoc allocates its wl_shm pixel buffers this way. */
	if (path[0] == '/' && path[1] == 'd' && path[2] == 'e' && path[3] == 'v' &&
	    path[4] == '/' && path[5] == 's' && path[6] == 'h' && path[7] == 'm' &&
	    path[8] == '/') {
		int fd = fd_alloc_kind(-1, 0, K_SHM);
		return fd < 0 ? -E_MFILE : fd;
	}
	if (path[0] == '/' && path[1] == 'd' && path[2] == 'e' &&
	    path[3] == 'v' && path[4] == '/' && path[5] == 'p' && path[6] == 't' &&
	    path[7] == 's' && path[8] == '/') {
		/* open("/dev/pts/N") — mint a FRESH slave cap for pty N each time, so it is
		 * REOPENABLE. openpty-based apps (xterm) open the slave once in the parent,
		 * close it, then reopen it by name in the forked child; a one-shot stash would
		 * fail that second open. The stash stays alive (the master fd holds the pty)
		 * and fork clones the handle table, so the child can reopen too. */
		int n = 0;
		for (const char *p = path + 9; *p >= '0' && *p <= '9'; p++)
			n = n * 10 + (*p - '0');
		for (int i = 0; i < 4; i++)
			if (g_pts[i].n == n && g_pts[i].slave >= 0) {
				long s = ox_pty_open_slave((unsigned int)g_pts[i].slave);
				if (s < 0)
					return -E_NOENT;
				int fd = fd_alloc_kind(s, 0, K_PTYS);
				return fd < 0 ? -E_MFILE : fd;
			}
		return -E_NOENT;
	}
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
		if (fds[fd].kind == K_SOCK) {
			if (fds[fd].handle < 0)
				return -E_INVAL;
			if (fds[fd].nonblock) {
				long n = __oxbow_sock_recv_nb(fds[fd].handle, buf, len);
				return (n == -11) ? -11 : n; /* -11 = EAGAIN (socket open, no data yet) */
			}
			return __oxbow_sock_recv(fds[fd].handle, buf, len);
		}
		if (fds[fd].kind == K_PTYM || fds[fd].kind == K_PTYS)
			return ox_pty_read((unsigned int)fds[fd].handle, buf, len);
		if (fds[fd].kind == K_CHAN) {
			/* §weston: plain read() on a channel fd = raw channel bytes (non-blocking).
			 * The oxbow input drivers push raw scancode/PS-2 byte streams over channels
			 * with NO caps; weston's input handlers read() them directly. (libwayland's
			 * display socket uses recvmsg instead, to also collect SCM_RIGHTS caps.) */
			unsigned int caps[8];
			long r = ox_chan_recv((unsigned int)fds[fd].handle, buf, len, caps, 8, 1);
			if (r < 0)
				return -11; /* EAGAIN: nothing buffered */
			return (long)((unsigned long)r & 0xffffffffUL); /* byte count (caps ignored) */
		}
		if (fds[fd].kind == K_TIMERFD) {
			/* §weston: return the u64 expiration count. One-shot: fire once when the
			 * deadline is reached, then disarm (or re-arm for an interval timer). */
			if (len < 8 || !buf)
				return -E_INVAL;
			unsigned long now = ox_uptime_ms();
			unsigned long long count = 0;
			if (fds[fd].off != 0 && now >= fds[fd].off) {
				count = 1;
				if (fds[fd].size)
					fds[fd].off = now + fds[fd].size; /* interval: re-arm */
				else
					fds[fd].off = 0; /* one-shot: disarm */
			}
			if (count == 0)
				return -11; /* EAGAIN: not expired yet */
			*(unsigned long long *)buf = count;
			return 8;
		}
		if (fds[fd].kind == K_EVENTFD) {
			if (len < 8 || !buf)
				return -E_INVAL;
			unsigned long long v = fds[fd].off;
			if (v == 0)
				return -11; /* EAGAIN */
			fds[fd].off = 0;
			*(unsigned long long *)buf = v;
			return 8;
		}
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
		if (fds[fd].kind == K_SOCK)
			return fds[fd].handle < 0 ? -E_INVAL
			                          : __oxbow_sock_send(fds[fd].handle, buf, len);
		if (fds[fd].kind == K_PTYM || fds[fd].kind == K_PTYS)
			return ox_pty_write((unsigned int)fds[fd].handle, buf, len);
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

/* Fill a `struct sockaddr_in` (16 bytes): sin_family=AF_INET @0, sin_port (net order)
 * @2, sin_addr (net/dotted order) @4, 8 zero pad @8. `ip` is packed big-endian
 * (a<<24|b<<16|c<<8|d), `port` host order. The pad MUST be zero — musl's resolver
 * memcmp's the reply source against the nameserver sockaddr, pad included. Updates
 * *addrlen to 16 if provided. */
static void fill_sockaddr_in(unsigned char *sa, unsigned int *addrlen,
                             unsigned int ip, unsigned short port)
{
	for (int i = 0; i < 16; i++)
		sa[i] = 0;
	sa[0] = LAF_INET & 0xff;        /* sin_family low byte  */
	sa[1] = (LAF_INET >> 8) & 0xff; /* sin_family high byte */
	sa[2] = (unsigned char)(port >> 8);   /* sin_port, network order */
	sa[3] = (unsigned char)(port & 0xff);
	sa[4] = (unsigned char)(ip >> 24);    /* sin_addr, network/dotted order */
	sa[5] = (unsigned char)(ip >> 16);
	sa[6] = (unsigned char)(ip >> 8);
	sa[7] = (unsigned char)(ip & 0xff);
	if (addrlen)
		*addrlen = 16;
}

/* Honest readiness for TCP listeners in select()/poll(). The net server's accept is
 * non-blocking, so to report a listener "readable" we PEEK by accepting now and stashing
 * the connection; the next accept() consumes the stash without blocking. One slot is
 * enough (a server has a single listener). Without this, a select-loop server like
 * darkhttpd would call accept() on an always-"ready" listener with nothing pending and
 * block — never servicing the connection it already holds. */
static struct {
	int active;
	int listener_fd;
	long sock;
	unsigned int ip;
	unsigned short port;
} accept_stash;

static int listener_pending(int fd)
{
	if (fd < 0 || fd >= MAXFD || !fds[fd].used || fds[fd].kind != K_LISTEN)
		return 0;
	if (accept_stash.active)
		return accept_stash.listener_fd == fd; /* single slot is taken */
	unsigned int ip = 0;
	unsigned short port = 0;
	long sock = __oxbow_sock_tcp_accept(fds[fd].handle, &ip, &port);
	if (sock < 0)
		return 0; /* nothing pending */
	accept_stash.active = 1;
	accept_stash.listener_fd = fd;
	accept_stash.sock = sock;
	accept_stash.ip = ip;
	accept_stash.port = port;
	return 1;
}

/* §weston: poll-style readiness for ONE fd. Returns which of the requested events
 * (bit0 IN, bit1 PRI, bit2 OUT) are ready. OUT is always ready (we never block on
 * write); IN is gated per fd-kind exactly like NR_poll, plus timerfd expiry. Shared by
 * NR_poll and epoll_wait so their readiness rules can't drift apart. */
static short fd_revents(int fd, short want)
{
	if (fd < 0)
		return 0;
	short re = (short)(want & 0x7);
	/* An untracked fd (std streams 0/1/2, or any fd not in our table) is treated as
	 * ready — matching the pre-block poll(), which never blocked on them (a blocking
	 * read()/write() on the std stream handles it). Only TRACKED fds get IN-gated. */
	if (fd < MAXFD && fds[fd].used && (re & 0x1)) {
		int kind = fds[fd].kind;
		if (kind == K_LISTEN && !listener_pending(fd))
			re &= ~0x1;
		else if (kind == K_CHAN && !(ox_chan_poll((unsigned int)fds[fd].handle) & 1))
			re &= ~0x1;
		else if ((kind == K_PTYM || kind == K_PTYS) &&
			 ox_pty_ioctl((unsigned int)fds[fd].handle, 0x100, 0) != 1)
			re &= ~0x1;
		else if (kind == K_SOCK && fds[fd].handle >= 0 &&
			 !__oxbow_sock_recv_ready(fds[fd].handle))
			re &= ~0x1;
		else if (kind == K_TIMERFD) {
			/* .off = absolute deadline ms (0 = disarmed); readable once reached. */
			if (fds[fd].off == 0 ||
			    ox_uptime_ms() < fds[fd].off)
				re &= ~0x1;
		} else if (kind == K_SIGNALFD)
			re &= ~0x1; /* stub: signals never fire */
		else if (kind == K_EVENTFD && fds[fd].off == 0)
			re &= ~0x1; /* readable only when the counter is nonzero */
	}
	return re;
}

/* §weston: epoll registration table (one epoll set watches many fds). Keyed by the
 * epoll fd's slot; a small fixed pool covers libwayland's event loop with headroom. */
#define MAX_EPOLL_REGS 128
static struct {
	int used;
	int epfd;                /* the epoll fd this registration belongs to */
	int fd;                  /* the watched fd */
	unsigned int events;     /* requested epoll events (EPOLLIN/EPOLLOUT/...) */
	unsigned long long data; /* opaque user data returned on readiness */
} g_epoll[MAX_EPOLL_REGS];

/* §responsiveness: the sleeping replacement for the busy-yield in poll/select/epoll. Given
 * the watched fds, gather their channel handles + the nearest timerfd deadline and block on
 * `ox_chan_wait` until a channel is readable or the deadline passes — so weston and its
 * clients stop spinning at 100% CPU (which starved fsd's seeding + everything else).
 * `hard_ms` = the caller's own timeout budget (<0 = infinite). Falls back to a single YIELD
 * when there are no channels to sleep on (e.g. a socket-only select), preserving old
 * behavior for those; caps the wait to 8ms when sockets/ptys are present so they still get
 * polled (ox_chan_wait can't wake on them). */
static void block_wait_fds(const int *fdlist, int nfd, long hard_ms)
{
	unsigned int chans[16];
	int nch = 0, has_other = 0, have_dl = 0;
	unsigned long deadline = 0;
	for (int i = 0; i < nfd; i++) {
		int fd = fdlist[i];
		if (fd < 0 || fd >= MAXFD || !fds[fd].used)
			continue;
		int k = fds[fd].kind;
		if (k == K_CHAN && fds[fd].handle >= 0 && nch < 16)
			chans[nch++] = (unsigned int)fds[fd].handle;
		else if (k == K_TIMERFD && fds[fd].off != 0) {
			if (!have_dl || fds[fd].off < deadline) {
				deadline = fds[fd].off;
				have_dl = 1;
			}
		} else if (k == K_SOCK || k == K_PTYM || k == K_PTYS || k == K_LISTEN)
			has_other = 1;
	}
	if (nch == 0) {
		ox_syscall0(OX_SYS_YIELD); /* nothing sleepable — keep old busy behavior */
		return;
	}
	unsigned long now = ox_uptime_ms();
	long wait_ms = -1; /* -1 = infinite */
	if (have_dl)
		wait_ms = (deadline > now) ? (long)(deadline - now) : 1;
	if (hard_ms >= 0 && (wait_ms < 0 || hard_ms < wait_ms))
		wait_ms = hard_ms;
	if (has_other && (wait_ms < 0 || wait_ms > 8))
		wait_ms = 8;
	if (wait_ms < 0)
		wait_ms = 0; /* 0 => ox_chan_wait blocks until a channel is readable */
	ox_chan_wait(chans, (unsigned long)nch, (unsigned long)wait_ms);
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
	if (fds[1].used && (fds[1].kind == K_PIPE_W || fds[1].kind == K_PTYS))
		stdout_cap = (unsigned int)fds[1].handle;
	/* Honor a dup2'd stdin too: popen("w") / a pipeline does dup2(pipe_r, 0) +
	 * exec, so the child reads its stdin from the pipe. A forkpty child dup2's the
	 * pty SLAVE onto 0/1/2 (login_tty), so it inherits the pty as its tty. 0 = ours. */
	unsigned int stdin_cap = 0;
	if (fds[0].used && (fds[0].kind == K_PIPE_R || fds[0].kind == K_PTYS))
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
		if (setjmp(fork_buf) != 0) {
			/* child: fork() returns 0, in its own AS. Its socket fds are BORROWED copies
			 * of the parent's caps (the kernel cloned the handle table by value); closing
			 * them must not tear the socket down for the still-running parent. The owner
			 * (parent) keeps owns=1 and does the real teardown. (Fixes xterm: its child
			 * close()s the X connection so the shell can't see it, which otherwise killed
			 * the parent's X socket. A long-lived child that needs to OWN an inherited
			 * socket — accept()+fork servers — would need real refcounting; none today.) */
			for (int i = 0; i < MAXFD; i++)
				if (fds[i].used &&
				    (fds[i].kind == K_SOCK || fds[i].kind == K_LISTEN || fds[i].kind == K_UDP))
					fds[i].owns = 0;
			return 0;
		}
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
			/* POSIX writev: a short write ends the call. A socket send can accept
			 * fewer bytes than asked (smoltcp TX buffer partly full); skipping to the
			 * next iovec would drop this iovec's tail and corrupt the byte stream
			 * (X protocol desync → lost events, e.g. twm never seeing a MapRequest).
			 * Stop here and let the caller retry the unsent remainder. */
			if ((unsigned long)w < iov[i].len)
				break;
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

	/* ---- BSD sockets ----
	 * TCP client (Phase 1): socket()->K_SOCK; connect() drives oxbow's capability TCP
	 *   API and stores the socket cap; read/write/send/recv route to it.
	 * UDP + DNS (Phase 2): socket(SOCK_DGRAM)->K_UDP; bind() binds an ephemeral UDP
	 *   socket; sendto()/recvmsg() carry the peer address. This is exactly what musl's
	 *   resolver uses — so unmodified getaddrinfo() resolves over real DNS.
	 * Only AF_INET is supported (SOCK_CLOEXEC/NONBLOCK type-flags are masked off). */
	case NR_socket: {
		long domain = a1, type = a2;
		if (domain != LAF_INET)
			return -E_AFNOSUPPORT;
		int t = (int)(type & LSOCK_TYPE_MASK);
		if (t == LSOCK_STREAM) {
			int fd = fd_alloc_kind(-1, 0, K_SOCK); /* -1 = not yet connected */
			return fd < 0 ? -E_MFILE : fd;
		}
		if (t == LSOCK_DGRAM) {
			int fd = fd_alloc_kind(-1, 0, K_UDP); /* -1 = not yet bound */
			return fd < 0 ? -E_MFILE : fd;
		}
		return -E_INVAL;
	}
	case NR_connect: {
		int fd = (int)a1;
		const unsigned char *sa = (const unsigned char *)a2;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used || fds[fd].kind != K_SOCK)
			return -E_BADF;
		if (!sa || a3 < 8)
			return -E_INVAL;
		/* sockaddr_in: sin_port @2 (net order), sin_addr @4 (net order = a.b.c.d). */
		unsigned short port = (unsigned short)((sa[2] << 8) | sa[3]);
		unsigned int ip = ((unsigned int)sa[4] << 24) | ((unsigned int)sa[5] << 16) |
		                  ((unsigned int)sa[6] << 8) | (unsigned int)sa[7];
		long h = __oxbow_sock_tcp_connect(ip, port);
		if (h < 0)
			return -E_CONNREFUSED;
		fds[fd].handle = h;
		return 0;
	}
	case NR_bind: {
		/* UDP: bind the socket now (wildcard; resolver uses port 0 = ephemeral).
		 * TCP: just remember the port in .off — listen() opens the listener with it. */
		int fd = (int)a1;
		const unsigned char *sa = (const unsigned char *)a2;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used)
			return -E_BADF;
		unsigned short port = (sa && a3 >= 8) ? (unsigned short)((sa[2] << 8) | sa[3]) : 0;
		if (fds[fd].kind == K_UDP) {
			long h = __oxbow_sock_udp_bind(port);
			if (h < 0)
				return -E_INVAL;
			fds[fd].handle = h;
			return 0;
		}
		if (fds[fd].kind == K_SOCK) {
			fds[fd].off = port; /* stash the listen port for listen() */
			return 0;
		}
		return 0;
	}
	case NR_listen: {
		/* Turn a bound TCP socket into a listener. The net server opens the listen
		 * port and badges a listener cap; accept() polls it. backlog (a2) is advisory. */
		int fd = (int)a1;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used || fds[fd].kind != K_SOCK)
			return -E_BADF;
		long h = __oxbow_sock_tcp_listen((unsigned short)fds[fd].off);
		if (h < 0)
			return -E_INVAL; /* port unavailable / no socket slot */
		fds[fd].handle = h;
		fds[fd].kind = K_LISTEN;
		return 0;
	}
	case NR_accept:
	case NR_accept4: {
		/* Consume a connection a prior select()/poll() already peeked (the common path
		 * for a select-loop server); otherwise block until a client connects (the net
		 * server's accept is non-blocking, so poll it with a yield). Then install the
		 * connected socket as a fresh K_SOCK fd and report the peer. accept4 flags ignored. */
		int fd = (int)a1;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used || fds[fd].kind != K_LISTEN)
			return -E_INVAL;
		unsigned int pip = 0;
		unsigned short pport = 0;
		long sock;
		if (accept_stash.active && accept_stash.listener_fd == fd) {
			sock = accept_stash.sock;
			pip = accept_stash.ip;
			pport = accept_stash.port;
			accept_stash.active = 0;
		} else {
			for (;;) {
				sock = __oxbow_sock_tcp_accept(fds[fd].handle, &pip, &pport);
				if (sock >= 0)
					break;
				ox_syscall0(OX_SYS_YIELD); /* nothing pending — yield, then retry */
			}
		}
		int nfd = fd_alloc_kind(sock, 0, K_SOCK);
		if (nfd < 0) {
			__oxbow_sock_close(sock);
			return -E_MFILE;
		}
		unsigned char *sa = (unsigned char *)a2;
		unsigned int *sl = (unsigned int *)a3;
		if (sa && sl && *sl >= 16)
			fill_sockaddr_in(sa, sl, pip, pport);
		return nfd;
	}
	case NR_sendto: {
		/* TCP (connected): ignore the address, just send. UDP: the destination rides
		 * in the sockaddr at a5 (len a6); auto-bind an ephemeral socket on first use. */
		int fd = (int)a1;
		if (fd >= 0 && fd < MAXFD && fds[fd].used && fds[fd].kind == K_UDP) {
			const unsigned char *sa = (const unsigned char *)a5;
			if (!sa || a6 < 8)
				return -E_INVAL;
			if (fds[fd].handle < 0) {
				long h = __oxbow_sock_udp_bind(0);
				if (h < 0)
					return -E_INVAL;
				fds[fd].handle = h;
			}
			unsigned short port = (unsigned short)((sa[2] << 8) | sa[3]);
			unsigned int ip = ((unsigned int)sa[4] << 24) | ((unsigned int)sa[5] << 16) |
			                  ((unsigned int)sa[6] << 8) | (unsigned int)sa[7];
			return __oxbow_sock_udp_sendto(fds[fd].handle, ip, port,
			                               (const void *)a2, (unsigned long)a3);
		}
		return do_write(a1, (const void *)a2, (unsigned long)a3);
	}
	case NR_recvfrom: {
		/* TCP: plain recv (src addr not filled). UDP: fill the sender's sockaddr_in. */
		int fd = (int)a1;
		if (fd >= 0 && fd < MAXFD && fds[fd].used && fds[fd].kind == K_UDP) {
			if (fds[fd].handle < 0)
				return -E_INVAL;
			unsigned int sip = 0;
			unsigned short sport = 0;
			long n = __oxbow_sock_udp_recvfrom(fds[fd].handle, (void *)a2,
			                                   (unsigned long)a3, &sip, &sport);
			if (a5)
				fill_sockaddr_in((unsigned char *)a5, (unsigned int *)a6, sip, sport);
			return n;
		}
		if (a5)
			*(unsigned int *)a6 = 0; /* TCP: report a 0-length source address */
		return do_read(a1, (void *)a2, (unsigned long)a3);
	}
	case NR_recvmsg: {
		/* musl's DNS resolver reads UDP replies via recvmsg (single iov + msg_name for
		 * the source address, which it validates). Support that shape for K_UDP. */
		int fd = (int)a1;
		struct msghdr_x {
			void *msg_name;
			unsigned int msg_namelen;
			unsigned int _pad;
			struct iovec_x { void *base; unsigned long len; } *msg_iov;
			unsigned long msg_iovlen;
			void *msg_control;
			unsigned long msg_controllen;
			int msg_flags;
		} *mh = (struct msghdr_x *)a2;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used)
			return -E_INVAL;
		if (!mh || !mh->msg_iov || mh->msg_iovlen < 1)
			return -E_INVAL;
		if (fds[fd].kind == K_CHAN) {
			/* §wayland: receive wire bytes (scattered into the iovs) + any passed
			 * capabilities, adopting each as a fresh shm fd reported via an SCM_RIGHTS
			 * cmsg. The display fd is O_NONBLOCK + poll-driven, so recv non-blocking. */
			char tmp[4096];
			unsigned int caps[8];
			long r = ox_chan_recv((unsigned int)fds[fd].handle, tmp, sizeof tmp, caps, 8, 1);
			if (r < 0)
				return -11; /* EAGAIN: nothing buffered */
			unsigned long nbytes = (unsigned long)r & 0xffffffffUL;
			unsigned long ncaps = ((unsigned long)r >> 32) & 0xffffffffUL;
			unsigned long copied = 0;
			for (unsigned long i = 0; i < mh->msg_iovlen && copied < nbytes; i++) {
				unsigned long take = mh->msg_iov[i].len;
				if (take > nbytes - copied)
					take = nbytes - copied;
				memcpy(mh->msg_iov[i].base, tmp + copied, take);
				copied += take;
			}
			if (ncaps && mh->msg_control && mh->msg_controllen >= 16 + ncaps * 4) {
				struct cmsg_x { unsigned long len; int level; int type; } *c = mh->msg_control;
				c->len = 16 + ncaps * 4;
				c->level = 1; /* SOL_SOCKET */
				c->type = 1;  /* SCM_RIGHTS */
				int *fp = (int *)((char *)mh->msg_control + 16);
				for (unsigned long i = 0; i < ncaps; i++)
					fp[i] = fd_alloc_kind((long)caps[i], 0, K_SHM);
				mh->msg_controllen = 16 + ncaps * 4;
			} else {
				mh->msg_controllen = 0;
			}
			mh->msg_flags = 0;
			return (long)copied;
		}
		if (fds[fd].kind == K_SOCK) {
			/* TCP stream recv (the X server reads client sockets via recvmsg because
			 * XTRANS_SEND_FDS is on). No fd passing on a TCP socket — just fill the iov.
			 * Honor O_NONBLOCK so a poll-driven reader gets EAGAIN, not a block. */
			if (fds[fd].handle < 0)
				return -E_INVAL;
			long n = fds[fd].nonblock
			    ? __oxbow_sock_recv_nb(fds[fd].handle, mh->msg_iov[0].base, mh->msg_iov[0].len)
			    : __oxbow_sock_recv(fds[fd].handle, mh->msg_iov[0].base, mh->msg_iov[0].len);
			if (n == -11)
				return -11; /* EAGAIN */
			if (n < 0)
				return n;
			mh->msg_controllen = 0;
			mh->msg_flags = 0;
			return n;
		}
		if (fds[fd].kind != K_UDP || fds[fd].handle < 0)
			return -E_INVAL;
		unsigned int sip = 0;
		unsigned short sport = 0;
		long n = __oxbow_sock_udp_recvfrom(fds[fd].handle, mh->msg_iov[0].base,
		                                   mh->msg_iov[0].len, &sip, &sport);
		if (n < 0)
			return n;
		if (mh->msg_name && mh->msg_namelen >= 16)
			fill_sockaddr_in((unsigned char *)mh->msg_name, &mh->msg_namelen, sip, sport);
		mh->msg_flags = 0; /* datagram fit (no MSG_TRUNC) — recvfrom caps at the iov len */
		return n;
	}
	case NR_sendmsg: {
		/* §wayland: the client sends wire requests + (for wl_shm) shm-pool fds via
		 * SCM_RIGHTS — translated to capability-handle passing over the channel. (UDP
		 * uses sendto(), not sendmsg, so K_CHAN is the only sendmsg shape we serve.) */
		int fd = (int)a1;
		struct msghdr_x {
			void *msg_name; unsigned int msg_namelen; unsigned int _pad;
			struct iovec_x { void *base; unsigned long len; } *msg_iov;
			unsigned long msg_iovlen;
			void *msg_control; unsigned long msg_controllen;
			int msg_flags;
		} *mh = (struct msghdr_x *)a2;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used || !mh || !mh->msg_iov)
			return -E_INVAL;
		if (fds[fd].kind == K_SOCK) {
			/* TCP stream send (the X server writes client sockets via sendmsg because
			 * XTRANS_SEND_FDS is on). No fd passing on a TCP socket — send each iov. */
			if (fds[fd].handle < 0)
				return -E_INVAL;
			long total = 0;
			for (unsigned long i = 0; i < mh->msg_iovlen; i++) {
				if (!mh->msg_iov[i].len)
					continue;
				long s = __oxbow_sock_send(fds[fd].handle, mh->msg_iov[i].base, mh->msg_iov[i].len);
				if (s < 0)
					return total > 0 ? total : s;
				total += s;
			}
			return total;
		}
		if (fds[fd].kind != K_CHAN)
			return -E_INVAL;
		char tmp[4096];
		unsigned long dlen = 0;
		for (unsigned long i = 0; i < mh->msg_iovlen && dlen < sizeof tmp; i++) {
			unsigned long take = mh->msg_iov[i].len;
			if (take > sizeof tmp - dlen)
				take = sizeof tmp - dlen;
			memcpy(tmp + dlen, mh->msg_iov[i].base, take);
			dlen += take;
		}
		unsigned int caps[8];
		unsigned long ncaps = 0;
		if (mh->msg_control && mh->msg_controllen >= 16) {
			struct cmsg_x { unsigned long len; int level; int type; } *c = mh->msg_control;
			if (c->level == 1 && c->type == 1) { /* SOL_SOCKET, SCM_RIGHTS */
				unsigned long nfd = (c->len - 16) / 4;
				int *fp = (int *)((char *)mh->msg_control + 16);
				for (unsigned long i = 0; i < nfd && ncaps < 8; i++) {
					int pfd = fp[i];
					if (pfd >= 0 && pfd < MAXFD && fds[pfd].used && fds[pfd].handle >= 0)
						caps[ncaps++] = (unsigned int)fds[pfd].handle;
				}
			}
		}
		long s = ox_chan_send((unsigned int)fds[fd].handle, tmp, dlen, caps, ncaps);
		if (s == 0 && dlen > 0)
			return -E_INVAL;
		return (long)dlen;
	}
	case NR_getsockname: {
		/* Report 0.0.0.0 + the socket's bound port (stashed in .off at bind). Enough for
		 * a server that getsockname()'s its listener to learn/print its own port. */
		int fd = (int)a1;
		unsigned char *sa = (unsigned char *)a2;
		unsigned int *sl = (unsigned int *)a3;
		unsigned short port = 0;
		if (fd >= 0 && fd < MAXFD && fds[fd].used &&
		    (fds[fd].kind == K_SOCK || fds[fd].kind == K_LISTEN))
			port = (unsigned short)fds[fd].off;
		if (sa && sl && *sl >= 16)
			fill_sockaddr_in(sa, sl, 0, port);
		return 0;
	}
	case NR_getpeername: {
		/* Report the peer as 127.0.0.1 + the socket's port. Our only TCP-server peers are
		 * local (X clients over loopback), and xtrans's SocketINETAccept aborts the
		 * connection if getpeername() fails — so this must succeed and read as localhost
		 * (which X access control allows). */
		int fd = (int)a1;
		unsigned char *sa = (unsigned char *)a2;
		unsigned int *sl = (unsigned int *)a3;
		unsigned short port = 0;
		if (fd >= 0 && fd < MAXFD && fds[fd].used &&
		    (fds[fd].kind == K_SOCK || fds[fd].kind == K_LISTEN))
			port = (unsigned short)fds[fd].off;
		if (sa && sl && *sl >= 16)
			fill_sockaddr_in(sa, sl, 0x7f000001u, port); /* 127.0.0.1 */
		return 0;
	}
	case NR_shutdown: /* half-close: treat as a no-op; close() does the teardown */
	case NR_setsockopt: /* accept option sets benignly (no real options yet) */
	case NR_getsockopt:
		return 0;
	case NR_memfd_create:
		/* §wayland: a wl_shm pool fd. Its shared backing region is allocated lazily by
		 * the subsequent ftruncate (which carries the size). */
		return fd_alloc_kind(-1, 0, K_SHM);
	case NR_fallocate: {
		/* fallocate(fd, mode, offset, len): posix_fallocate() sizes a memfd here.
		 * weston's os_create_anonymous_file uses it (not ftruncate) to allocate the
		 * keymap memfd — so a K_SHM fd must allocate its backing region, same as
		 * ftruncate. New size = offset + len. Returns 0 (musl posix_fallocate wants 0). */
		long fd = a1;
		unsigned long need = (unsigned long)a3 + (unsigned long)a4; /* offset + len */
		if (fd < 0 || fd >= MAXFD || !fds[fd].used)
			return -E_BADF;
		if (fds[fd].kind == K_SHM) {
			if (fds[fd].handle < 0 && need > 0) {
				unsigned long pages = (need + 4095) / 4096;
				long h = ox_shm_create(pages);
				if (h < 0)
					return -12; /* ENOMEM */
				fds[fd].handle = h;
				fds[fd].size = need;
			}
			return 0;
		}
		return 0; /* non-shm: accept (best-effort; real allocation not needed) */
	}
	case NR_ftruncate: {
		/* ftruncate(fd, len): set a file's length. Editors (kilo) rewrite a file as
		 * ftruncate(len) + write(len); without this, the dirty flag never clears. */
		long fd = a1;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used)
			return -E_BADF;
		if (fds[fd].kind == K_SHM) {
			/* §wayland: sizing a wl_shm memfd allocates its backing shm region. */
			if (fds[fd].handle < 0 && (long)a2 > 0) {
				unsigned long pages = (((unsigned long)a2) + 4095) / 4096;
				long h = ox_shm_create(pages);
				if (h < 0)
					return -12; /* ENOMEM */
				fds[fd].handle = h;
				fds[fd].size = (unsigned long)a2;
			}
			return 0;
		}
		if (fds[fd].kind != K_FILE)
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
	case NR_mmap: {
		/* §wayland: file-backed mmap of a wl_shm memfd maps the SHARED region's frames
		 * (so the client and the compositor — both holding the Shm cap — see the same
		 * pixels). a5 = fd, a2 = len. */
		int mfd = (int)a5;
		if (mfd >= 0 && mfd < MAXFD && fds[mfd].used && fds[mfd].kind == K_SHM &&
		    fds[mfd].handle >= 0) {
			unsigned long len = ((unsigned long)a2 + 4095) & ~4095UL;
			unsigned long va = g_shm_next;
			g_shm_next += len;
			if (ox_shm_map((unsigned int)fds[mfd].handle, va) == 0)
				return -12; /* ENOMEM */
			/* §41: the mapping now owns a reference to the shm region; hold the cap
			 * (keyed by va) so close(fd) doesn't drop it — munmap will. */
			shm_map_track(va, fds[mfd].handle);
			return (long)va;
		}
		/* __oxbow_mmap_anon always maps RW (ignoring PROT_NONE); musl's mallocng
		 * mmaps PROT_NONE then mprotects to RW, which we make a no-op below. */
		return (long)__oxbow_mmap_anon((unsigned long)a2);
	}
	case NR_munmap: {
		/* §41: untrack this wl_shm mapping. Free the region ONLY if no open fd still
		 * references it — POSIX munmap unmaps but does NOT destroy a memfd; only close()
		 * frees it. weston's ro-anonymous-file munmaps its keymap memfd yet keeps the fd
		 * to hand to clients, so a live fd must keep the region alive. wl_shm's opposite
		 * pattern (close the pool fd early, keep the mapping) already works: after that
		 * close no fd holds the region, so munmap here frees it. Region lives until BOTH
		 * the fds and the mappings are gone. (Non-shm anon mapping: untracked → no-op.) */
		long h = shm_map_untrack((unsigned long)a1);
		if (h >= 0) {
			int fd_open = 0;
			for (int i = 0; i < MAXFD; i++)
				if (fds[i].used && fds[i].kind == K_SHM && fds[i].handle == h) {
					fd_open = 1;
					break;
				}
			if (!fd_open)
				ox_syscall1(OX_SYS_CLOSE, h);
		}
		return 0;
	}
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
	/* No interval timers on oxbow. Report "no alarm was pending" (0) rather than ENOSYS:
	 * apps like xterm call alarm(0) to cancel a watchdog and then inspect errno — an
	 * ENOSYS here would clobber a prior call's errno and be misread as a real failure. */
	case NR_alarm:
		return 0;
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
		/* §pty: ioctls on a pty master/slave fd (these are real ttys). */
		if (fd >= 3 && fd < MAXFD && fds[fd].used &&
		    (fds[fd].kind == K_PTYM || fds[fd].kind == K_PTYS)) {
			unsigned int h = (unsigned int)fds[fd].handle;
			if (fds[fd].kind == K_PTYM) {
				if (req == TIOCGPTN) {
					if (arg)
						*(int *)arg = (int)fds[fd].off;
					return 0;
				}
				if (req == TIOCSPTLCK)
					return 0; /* unlock: noop */
			}
			if (req == TCGETS || req == TCSETS || req == TCSETSW || req == TCSETSF ||
			    req == TIOCGWINSZ || req == TIOCSWINSZ || req == TIOCSCTTY)
				return ox_pty_ioctl(h, req, (unsigned long)arg) < 0 ? -E_INVAL : 0;
			return 0; /* other tty ioctls on a pty: benign success */
		}
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
	/* oxbow is single-user with no uid/privilege model; setting the (effective) user/group
	 * id or supplementary groups is a no-op SUCCESS. Apps like xterm drop privileges here and
	 * treat ENOSYS as fatal (ERROR_SETUID), so we must report success, not "unimplemented". */
	case NR_setuid:
	case NR_setgid:
	case NR_setreuid:
	case NR_setregid:
	case NR_setresuid:
	case NR_setresgid:
	case NR_setgroups:
	case NR_setfsuid:
	case NR_setfsgid:
		return 0;

	/* ---- exit ---- */
	case NR_exit:
	case NR_exit_group:
		ox_syscall1(OX_SYS_EXIT, a1);
		__builtin_unreachable();

	/* ---- I/O multiplexing (pragmatic: report the requested fds ready, so a
	 * poll/select-then-read works — reads block for real data) ---- */
	/* ---- epoll / timerfd / signalfd / eventfd — libwayland's server event loop ---- */
	case NR_epoll_create:
	case NR_epoll_create1: /* flags (CLOEXEC) ignored */
		return fd_alloc_kind(-1, 0, K_EPOLL);
	case NR_epoll_ctl: {
		int epfd = (int)a1, op = (int)a2, fd = (int)a3;
		struct lepoll_event { unsigned int events; unsigned long long data; }
			__attribute__((packed)) *ev = (struct lepoll_event *)a4;
		if (epfd < 0 || epfd >= MAXFD || !fds[epfd].used || fds[epfd].kind != K_EPOLL)
			return -E_BADF;
		if (op == LEPOLL_CTL_ADD) {
			int slot = -1;
			for (int i = 0; i < MAX_EPOLL_REGS; i++) {
				if (g_epoll[i].used && g_epoll[i].epfd == epfd && g_epoll[i].fd == fd) {
					slot = i;
					break;
				}
				if (slot < 0 && !g_epoll[i].used)
					slot = i;
			}
			if (slot < 0)
				return -12; /* ENOMEM */
			g_epoll[slot].used = 1;
			g_epoll[slot].epfd = epfd;
			g_epoll[slot].fd = fd;
			g_epoll[slot].events = ev ? ev->events : 0;
			g_epoll[slot].data = ev ? ev->data : 0;
			return 0;
		} else if (op == LEPOLL_CTL_MOD) {
			for (int i = 0; i < MAX_EPOLL_REGS; i++)
				if (g_epoll[i].used && g_epoll[i].epfd == epfd && g_epoll[i].fd == fd) {
					g_epoll[i].events = ev ? ev->events : 0;
					g_epoll[i].data = ev ? ev->data : 0;
					return 0;
				}
			return -E_INVAL; /* ENOENT */
		} else if (op == LEPOLL_CTL_DEL) {
			for (int i = 0; i < MAX_EPOLL_REGS; i++)
				if (g_epoll[i].used && g_epoll[i].epfd == epfd && g_epoll[i].fd == fd)
					g_epoll[i].used = 0;
			return 0;
		}
		return -E_INVAL;
	}
	case NR_epoll_wait:
	case NR_epoll_pwait: {
		int epfd = (int)a1;
		struct lepoll_event { unsigned int events; unsigned long long data; }
			__attribute__((packed)) *out = (struct lepoll_event *)a2;
		int maxevents = (int)a3;
		int timeout = (int)a4; /* ms; -1 = forever, 0 = immediate */
		if (epfd < 0 || epfd >= MAXFD || !fds[epfd].used || fds[epfd].kind != K_EPOLL)
			return -E_BADF;
		unsigned long start = ox_uptime_ms();
		for (;;) {
			int n = 0;
			for (int i = 0; i < MAX_EPOLL_REGS && n < maxevents; i++) {
				if (!g_epoll[i].used || g_epoll[i].epfd != epfd)
					continue;
				short want = 0;
				if (g_epoll[i].events & LEPOLLIN)
					want |= 0x1;
				if (g_epoll[i].events & LEPOLLOUT)
					want |= 0x4;
				short re = fd_revents(g_epoll[i].fd, want);
				if (re) {
					unsigned int oe = 0;
					if (re & 0x1)
						oe |= LEPOLLIN;
					if (re & 0x4)
						oe |= LEPOLLOUT;
					if (out) {
						out[n].events = oe;
						out[n].data = g_epoll[i].data;
					}
					n++;
				}
			}
			if (n > 0)
				return n;
			if (timeout == 0)
				return 0;
			/* Sleep instead of spin: gather the watched fds and block until one is
			 * readable or the nearest timer fires (was a busy-yield that pinned a core). */
			int fdlist[MAX_EPOLL_REGS];
			int nfd = 0;
			for (int i = 0; i < MAX_EPOLL_REGS; i++)
				if (g_epoll[i].used && g_epoll[i].epfd == epfd)
					fdlist[nfd++] = g_epoll[i].fd;
			long hard = -1;
			if (timeout > 0) {
				unsigned long el = ox_uptime_ms() - start;
				hard = (timeout > (long)el) ? (timeout - (long)el) : 0;
			}
			block_wait_fds(fdlist, nfd, hard);
			if (timeout > 0 && ox_uptime_ms() - start >=
					    (unsigned long)timeout)
				return 0;
		}
	}
	case NR_timerfd_create:
		return fd_alloc_kind(-1, 0, K_TIMERFD); /* .off = deadline ms, .size = interval ms */
	case NR_timerfd_settime: {
		int fd = (int)a1;
		int flags = (int)a2; /* bit0 = TFD_TIMER_ABSTIME */
		struct ltimespec { long sec; long nsec; };
		struct litimerspec { struct ltimespec it_interval, it_value; } *nv =
			(struct litimerspec *)a3;
		if (fd < 0 || fd >= MAXFD || !fds[fd].used || fds[fd].kind != K_TIMERFD)
			return -E_BADF;
		if (!nv)
			return -E_FAULT;
		unsigned long now = ox_uptime_ms();
		unsigned long value_ms = (unsigned long)nv->it_value.sec * 1000UL +
					 (unsigned long)nv->it_value.nsec / 1000000UL;
		unsigned long interval_ms = (unsigned long)nv->it_interval.sec * 1000UL +
					    (unsigned long)nv->it_interval.nsec / 1000000UL;
		if (nv->it_value.sec == 0 && nv->it_value.nsec == 0)
			fds[fd].off = 0; /* disarm */
		else
			fds[fd].off = (flags & 1) ? value_ms : now + value_ms;
		fds[fd].size = interval_ms;
		return 0; /* old value (a4) not reported */
	}
	case NR_signalfd:
	case NR_signalfd4:
		return fd_alloc_kind(-1, 0, K_SIGNALFD); /* stub: never readable */
	case NR_eventfd:
	case NR_eventfd2: {
		int fd = fd_alloc_kind(-1, 0, K_EVENTFD);
		if (fd >= 0)
			fds[fd].off = (unsigned long)a1; /* initval */
		return fd;
	}
	case NR_poll:
	case NR_ppoll: {
		struct pfd {
			int fd;
			short events;
			short revents;
		} *pf = (struct pfd *)a1;
		unsigned long nfds = (unsigned long)a2;
		/* poll: a3 = int ms (-1 forever, 0 immediate). ppoll: a3 = struct timespec* (NULL
		 * = forever). Previously ignored → single-shot busy-poll; now we actually block. */
		long timeout_ms;
		if (n == NR_poll) {
			timeout_ms = (int)a3;
		} else {
			struct ltspec { long sec, nsec; } *ts = (struct ltspec *)a3;
			timeout_ms = ts ? (ts->sec * 1000 + ts->nsec / 1000000) : -1;
		}
		unsigned long start = ox_uptime_ms();
		for (;;) {
			int ready = 0, nfd = 0;
			int fdlist[64];
			for (unsigned long i = 0; i < nfds && pf; i++) {
				short re = fd_revents(pf[i].fd, pf[i].events);
				pf[i].revents = re;
				if (re)
					ready++;
				if (pf[i].fd >= 0 && nfd < 64)
					fdlist[nfd++] = pf[i].fd;
			}
			if (ready > 0)
				return ready;
			if (timeout_ms == 0)
				return 0;
			long hard = -1;
			if (timeout_ms > 0) {
				unsigned long el = ox_uptime_ms() - start;
				hard = (timeout_ms > (long)el) ? (timeout_ms - (long)el) : 0;
			}
			block_wait_fds(fdlist, nfd, hard);
			if (timeout_ms > 0 && ox_uptime_ms() - start >= (unsigned long)timeout_ms)
				return 0;
		}
	}
	case NR_select:
	case NR_pselect6: {
		/* nfds=a1; readfds=a2, writefds=a3, exceptfds=a4, timeout=a5 (NULL => block
		 * forever). Report requested fds ready, EXCEPT a listener in readfds is "ready"
		 * only when a connection is actually pending (peek + stash, so the following
		 * accept() doesn't block). A select-loop server (darkhttpd) passes a NULL timeout
		 * when idle and treats a 0 return as fatal — so with NULL timeout and nothing
		 * ready we must BLOCK (yield-loop) until something becomes ready, not return 0. */
		int nfds = (int)a1;
		unsigned long *rd = (unsigned long *)a2;
		unsigned long *wr = (unsigned long *)a3;
		unsigned long *ex = (unsigned long *)a4;
		const void *timeout = (const void *)a5; /* a5 == 0 => NULL => block forever */
		int nwords = (nfds + 63) / 64;
		if (nwords > 16)
			nwords = 16;
		unsigned long saved_rd[16], saved_wr[16];
		for (int w = 0; w < nwords; w++) {
			saved_rd[w] = rd ? rd[w] : 0;
			saved_wr[w] = wr ? wr[w] : 0;
		}
		int ready = 0;
		for (;;) {
			ready = 0;
			for (int w = 0; w < nwords; w++) {
				if (rd)
					rd[w] = saved_rd[w];
				if (wr)
					wr[w] = saved_wr[w];
			}
			for (int fd = 0; fd < nfds && fd < 1024; fd++) {
				unsigned long bit = 1UL << (fd & 63);
				int w = fd >> 6;
				if (rd && (saved_rd[w] & bit)) {
					if (fd < MAXFD && fds[fd].used && fds[fd].kind == K_LISTEN &&
					    !listener_pending(fd))
						rd[w] &= ~bit; /* listener, nothing pending -> not ready */
					/* §sock: a connected TCP socket is readable only when a recv would
					 * return immediately — so twm (libXt uses select, not poll) blocks in
					 * this yield-loop instead of a blocking recv that pins the single-
					 * threaded net server and starves the peer's (Xwayland) send. */
					else if (fd < MAXFD && fds[fd].used && fds[fd].kind == K_SOCK &&
						 fds[fd].handle >= 0 && !__oxbow_sock_recv_ready(fds[fd].handle))
						rd[w] &= ~bit;
					else
						ready++;
				}
				if (wr && (saved_wr[w] & bit))
					ready++;
			}
			if (ready > 0)
				break;
			/* Sleep instead of spin: block on the watched read fds' channels until one
			 * is readable (NULL timeout) or ~10ms (finite), rather than busy-yielding. */
			int fdlist[64];
			int nfd = 0;
			for (int fd = 0; fd < nfds && fd < 1024 && nfd < 64; fd++)
				if (rd && (saved_rd[fd >> 6] & (1UL << (fd & 63))))
					fdlist[nfd++] = fd;
			block_wait_fds(fdlist, nfd, timeout ? 10 : -1);
			if (timeout != 0)
				break; /* a finite/zero timeout was requested: return 0 now */
			/* else NULL timeout: keep blocking until a fd becomes ready */
		}
		if (ex)
			for (int w = 0; w < nwords; w++)
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
		case F_ADD_SEALS:
			/* §wayland: record memfd seals. weston seals its keymap memfd read-only
			 * (READONLY_SEALS) so os_ro_anonymous_file_{get,put}_fd hands the SAME fd to
			 * clients (v7+ PRIVATE path) and does NOT close it before libwayland flushes
			 * — without this the keymap's SCM_RIGHTS fd is closed early and lost. */
			if (fd >= 0 && fd < MAXFD && fds[fd].used)
				fds[fd].seals |= (unsigned int)arg;
			return 0;
		case F_GET_SEALS:
			if (fd >= 0 && fd < MAXFD && fds[fd].used)
				return (long)fds[fd].seals;
			return 0;
		case F_GETFD:
			return 0; /* no FD_CLOEXEC tracked */
		case F_SETFD:
			return 0; /* accept (CLOEXEC not enforced) */
		case F_GETFL:
			return 2; /* O_RDWR */
		case F_SETFL:
			/* Track O_NONBLOCK for sockets — a poll-driven peer (X client, Xwayland)
			 * needs read() to return EAGAIN, not block, so the single-threaded net
			 * server stays free to serve the other end over loopback. */
			if (fd >= 0 && fd < MAXFD && fds[fd].used)
				fds[fd].nonblock = (arg & 04000) ? 1 : 0; /* O_NONBLOCK (x86_64) */
			return 0;
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
