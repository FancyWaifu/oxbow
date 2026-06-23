/* Test program for the oxbow musl personality. Stock C against musl's headers.
 *   Phase 1: printf + exit.   Phase 2: heap + buffered stdio + file I/O.
 *   Phase 3: fork + execve + waitpid.   Phase 4: termios (isatty/winsize/raw). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/ioctl.h>
#include <termios.h>
#include <signal.h>
#include <fcntl.h>
#include <unistd.h>
#include <dirent.h>
#include <poll.h>
#include <sched.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <netdb.h>

static volatile sig_atomic_t got_sig = 0;
static void on_usr1(int s) { got_sig = s; }

/* Phase 1 (sockets): an unmodified-stock-musl TCP client. `muslhello net <ip> <port>`
 * opens a TCP connection, sends an HTTP GET, and prints the response — proving the
 * socket()/connect()/write()/read() syscalls route through the personality onto
 * oxbow's capability TCP stack. Needs a `net` grant (root has it). */
static int
net_test(const char *ipstr, int port)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	int s = socket(AF_INET, SOCK_STREAM, 0);
	if (s < 0) {
		printf("[net] socket() failed: %d\n", s);
		return 1;
	}
	struct sockaddr_in sa;
	memset(&sa, 0, sizeof sa);
	sa.sin_family = AF_INET;
	sa.sin_port = htons((unsigned short)port);
	sa.sin_addr.s_addr = inet_addr(ipstr);
	printf("[net] connecting to %s:%d ...\n", ipstr, port);
	if (connect(s, (struct sockaddr *)&sa, sizeof sa) != 0) {
		printf("[net] connect failed\n");
		close(s);
		return 1;
	}
	printf("[net] connected; sending HTTP GET\n");
	const char *req = "GET / HTTP/1.0\r\nHost: oxbow\r\nConnection: close\r\n\r\n";
	if (write(s, req, strlen(req)) < 0) {
		printf("[net] write failed\n");
		close(s);
		return 1;
	}
	char buf[1024];
	int total = 0, n;
	while ((n = read(s, buf, sizeof buf - 1)) > 0) {
		buf[n] = 0;
		fwrite(buf, 1, (size_t)n, stdout);
		total += n;
		if (total > 4000)
			break; /* cap the echoed response */
	}
	printf("\n[net] received %d bytes total — socket round-trip OK\n", total);
	close(s);
	return total > 0 ? 0 : 1;
}

/* Phase 2 (sockets): stock-musl getaddrinfo() — proves DNS over real UDP. musl's
 * resolver reads /etc/resolv.conf, opens a UDP socket, and sendto/recvmsg's the
 * nameserver (10.0.2.3 under QEMU SLIRP, which forwards to the host's resolver).
 * `muslhello dns <host> [port]` resolves <host>, prints the address(es), and — if a
 * port is given — connects + fetches over HTTP using the first result. */
static int
dns_test(const char *host, int port)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	struct addrinfo hints, *res = NULL;
	memset(&hints, 0, sizeof hints);
	hints.ai_family = AF_INET;     /* A records only -> a single DNS query */
	hints.ai_socktype = SOCK_STREAM;
	printf("[dns] resolving %s ...\n", host);
	int rc = getaddrinfo(host, NULL, &hints, &res);
	if (rc != 0) {
		printf("[dns] getaddrinfo(%s) failed: rc=%d\n", host, rc);
		return 1;
	}
	int count = 0;
	char first[32] = {0};
	for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
		struct sockaddr_in *sin = (struct sockaddr_in *)ai->ai_addr;
		unsigned char *a = (unsigned char *)&sin->sin_addr;
		printf("[dns] %s -> %d.%d.%d.%d\n", host, a[0], a[1], a[2], a[3]);
		if (count == 0)
			snprintf(first, sizeof first, "%d.%d.%d.%d", a[0], a[1], a[2], a[3]);
		count++;
	}
	freeaddrinfo(res);
	if (count == 0) {
		printf("[dns] no addresses\n");
		return 1;
	}
	printf("[dns] resolved OK (%d address(es))\n", count);
	if (port > 0)
		return net_test(first, port);
	return 0;
}

/* Phase 3 (sockets): a stock-musl TCP SERVER. `muslhello serve <port>` does the full
 * server path — socket/bind/listen/accept/read/write/close — then serves ONE request and
 * exits. Proves the personality's accept/listen onto oxbow's capability TCP listener.
 * Reach it from the host via a QEMU hostfwd (host:PORT -> guest:<port>). */
static int
serve_test(int port)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	int ls = socket(AF_INET, SOCK_STREAM, 0);
	if (ls < 0) {
		printf("[srv] socket() failed: %d\n", ls);
		return 1;
	}
	struct sockaddr_in sa;
	memset(&sa, 0, sizeof sa);
	sa.sin_family = AF_INET;
	sa.sin_port = htons((unsigned short)port);
	sa.sin_addr.s_addr = INADDR_ANY;
	if (bind(ls, (struct sockaddr *)&sa, sizeof sa) != 0) {
		printf("[srv] bind failed\n");
		return 1;
	}
	if (listen(ls, 4) != 0) {
		printf("[srv] listen failed\n");
		return 1;
	}
	printf("[srv] listening on port %d\n", port);
	struct sockaddr_in peer;
	socklen_t pl = sizeof peer;
	int cs = accept(ls, (struct sockaddr *)&peer, &pl);
	if (cs < 0) {
		printf("[srv] accept failed\n");
		close(ls);
		return 1;
	}
	unsigned char *pa = (unsigned char *)&peer.sin_addr;
	printf("[srv] accepted from %d.%d.%d.%d:%d\n", pa[0], pa[1], pa[2], pa[3],
	       ntohs(peer.sin_port));
	char buf[512];
	int n = read(cs, buf, sizeof buf - 1);
	if (n > 0) {
		buf[n] = 0;
		char *eol = strpbrk(buf, "\r\n");
		if (eol)
			*eol = 0;
		printf("[srv] request: %s\n", buf);
	}
	const char *body = "hello-from-oxbow-musl";
	const char *resp = "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n"
	                   "Content-Length: 21\r\nConnection: close\r\n\r\nhello-from-oxbow-musl";
	(void)body;
	write(cs, resp, strlen(resp));
	close(cs);
	close(ls);
	printf("[srv] served one request OK\n");
	return 0;
}

static volatile sig_atomic_t got_int = 0;
static void on_int(int s) { got_int = s; }

/* Phase 6: interactive stdin + Ctrl-C, driven by `muslhello tty`. Reads a line from
 * the keyboard, then installs a SIGINT handler and blocks on another read so a
 * Ctrl-C can be delivered to it. */
static int
tty_test(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	char line[128];
	printf("musltty: type a line + Enter:\n");
	if (fgets(line, sizeof line, stdin))
		printf("  you typed: %s", line); /* fgets keeps the trailing \n */
	else
		printf("  (EOF)\n");

	signal(SIGINT, on_int);
	printf("musltty: now press Ctrl-C:\n");
	char l2[128];
	char *r = fgets(l2, sizeof l2, stdin);
	if (got_int)
		printf("  caught SIGINT (sig=%d)\n", (int)got_int);
	else if (r)
		printf("  line2: %s", l2);
	else
		printf("  (no signal, EOF)\n");
	return 0;
}

static volatile sig_atomic_t async_int = 0;
static void on_async_int(int s) { async_int = s; }

/* Phase 9 step 2: a CPU-bound loop with a SIGINT handler. Run as `muslhello loop`,
 * then Ctrl-C — the kernel injects the handler asynchronously (the program is NOT at
 * a read boundary), it sets the flag, and the loop exits. Proves async delivery. */
static int
loop_test(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	signal(SIGINT, on_async_int);
	printf("musltty: looping (CPU-bound) — press Ctrl-C\n");
	long spins = 0;
	while (!async_int) {
		for (volatile long i = 0; i < 200000; i++) { /* CPU work */
		}
		sched_yield(); /* a syscall return — the async injection point */
		spins++;
	}
	printf("  caught async SIGINT (sig=%d) after %ld spins — handler ran!\n",
	       (int)async_int, spins);
	return 0;
}

int
main(int argc, char **argv)
{
	if (argc > 1 && strcmp(argv[1], "tty") == 0)
		return tty_test();
	if (argc > 1 && strcmp(argv[1], "loop") == 0)
		return loop_test();
	if (argc > 1 && strcmp(argv[1], "net") == 0) {
		const char *ip = argc > 2 ? argv[2] : "10.0.2.2";
		int port = argc > 3 ? atoi(argv[3]) : 80;
		return net_test(ip, port);
	}
	if (argc > 1 && strcmp(argv[1], "dns") == 0) {
		const char *host = argc > 2 ? argv[2] : "example.com";
		int port = argc > 3 ? atoi(argv[3]) : 0;
		return dns_test(host, port);
	}
	if (argc > 1 && strcmp(argv[1], "serve") == 0) {
		int port = argc > 2 ? atoi(argv[2]) : 8080;
		return serve_test(port);
	}
	setvbuf(stdout, NULL, _IONBF, 0); /* unbuffered, so output order is deterministic */
	printf("Hello from musl libc, running on oxbow!\n");

	/* heap + buffered file read (Phase 2) */
	struct stat st;
	if (stat("/hello.c", &st) == 0)
		printf("  stat(/hello.c): %lld bytes\n", (long long)st.st_size);
	FILE *f = fopen("/hello.c", "r");
	if (f) {
		char line[128];
		if (fgets(line, sizeof line, f))
			printf("  first line: %s", line);
		fclose(f);
	}

	/* Phase 4: termios — isatty, window size, and a raw-mode round-trip. */
	printf("  --- termios ---\n");
	printf("  isatty(1) = %d (expect 1)\n", isatty(1));
	struct winsize ws;
	if (ioctl(1, TIOCGWINSZ, &ws) == 0)
		printf("  winsize: %d rows x %d cols\n", ws.ws_row, ws.ws_col);
	struct termios tio;
	if (tcgetattr(0, &tio) == 0) {
		printf("  tcgetattr: ICANON=%d ECHO=%d ISIG=%d\n", !!(tio.c_lflag & ICANON),
		       !!(tio.c_lflag & ECHO), !!(tio.c_lflag & ISIG));
		struct termios raw = tio;
		raw.c_lflag &= ~(ICANON | ECHO);
		printf("  tcsetattr(raw) = %d\n", tcsetattr(0, TCSANOW, &raw));
	}

	/* Phase 4: signals — install a handler, raise it, confirm it ran. */
	printf("  --- signals ---\n");
	signal(SIGUSR1, on_usr1);
	raise(SIGUSR1);
	printf("  raise(SIGUSR1) -> handler saw sig=%d (expect %d)\n", (int)got_sig, SIGUSR1);

	/* Phase 3b: fork + waitpid status propagation. Child exits 42 in its OWN AS;
	 * the parent must read exactly 42 — proves fork + independent child + waitpid +
	 * exit-code carry (not a default-0 coincidence). */
	printf("  --- fork + _exit(42) ---\n");
	pid_t p1 = fork();
	if (p1 == 0)
		_exit(42);
	int st1 = 0;
	pid_t w1 = waitpid(p1, &st1, 0);
	printf("  fork#1: pid=%d waited=%d exit=%d (expect 42)\n", (int)p1, (int)w1, WEXITSTATUS(st1));

	/* fork + execve: the child runs /bin/seq (its stdout is ours), parent reaps it. */
	printf("  --- fork + exec `seq 1 5` ---\n");
	pid_t p2 = fork();
	if (p2 == 0) {
		char *args[] = { "seq", "1", "5", NULL };
		char *envp[] = { NULL };
		execve("/bin/seq", args, envp);
		_exit(127);
	}
	int st2 = 0;
	pid_t w2 = waitpid(p2, &st2, 0);
	printf("  fork#2: pid=%d waited=%d exit=%d\n", (int)p2, (int)w2, WEXITSTATUS(st2));

	/* Phase 6: pipe() + fork() — child writes, parent reads (POSIX shared pipe). */
	printf("  --- pipe + fork ---\n");
	int pfd[2];
	if (pipe(pfd) == 0) {
		pid_t pc = fork();
		if (pc == 0) {
			close(pfd[0]);
			write(pfd[1], "ping", 4);
			close(pfd[1]);
			_exit(0);
		}
		close(pfd[1]);
		char pb[16];
		int pn = read(pfd[0], pb, sizeof pb - 1);
		pb[pn > 0 ? pn : 0] = 0;
		close(pfd[0]);
		int ws;
		waitpid(pc, &ws, 0);
		printf("  pipe got: \"%s\" (%d bytes, expect ping)\n", pb, pn);
	}

	/* Phase 6: dup2 — redirect a pipe read end onto fd 7, read via that fd. */
	printf("  --- dup2 ---\n");
	int d[2];
	if (pipe(d) == 0) {
		write(d[1], "dup2ok", 6);
		close(d[1]);
		dup2(d[0], 7);
		close(d[0]);
		char db[16];
		int dn = read(7, db, sizeof db - 1);
		db[dn > 0 ? dn : 0] = 0;
		close(7);
		printf("  dup2(pipe,7) read: \"%s\" (expect dup2ok)\n", db);
	}

	/* Phase 8: getdents via opendir/readdir — list "/". */
	printf("  --- opendir/readdir(/) ---\n");
	DIR *dir = opendir("/");
	if (dir) {
		struct dirent *e;
		int n = 0;
		printf("  /:");
		while ((e = readdir(dir)) != NULL && n < 24) {
			printf(" %s", e->d_name);
			n++;
		}
		printf("\n");
		closedir(dir);
	} else {
		printf("  opendir(/) failed\n");
	}

	/* Phase 8: poll a pipe that has data — expect ready + POLLIN. */
	printf("  --- poll ---\n");
	int qp[2];
	if (pipe(qp) == 0) {
		write(qp[1], "x", 1);
		struct pollfd pfd = { .fd = qp[0], .events = POLLIN, .revents = 0 };
		int pr = poll(&pfd, 1, 0);
		printf("  poll -> %d revents&POLLIN=%d (expect 1 1)\n", pr, !!(pfd.revents & POLLIN));
		close(qp[0]);
		close(qp[1]);
	}

	/* Phase 8: execve stdin-redirect — pipe + fork + dup2(pipe,0) + exec /bin/cat,
	 * then write to the pipe; cat reads its stdin (the pipe) and echoes to our
	 * stdout. The popen("w") / pipeline-to-subprocess path. The child closes BOTH pipe
	 * ends after dup2 (standard) — with the kernel's writer-refcount (§Phase 11) that
	 * no longer races the parent; the pipe EOFs only when the last write end (the
	 * parent's) closes. */
	printf("  --- exec with redirected stdin (cat) ---\n");
	int cp[2];
	if (pipe(cp) == 0) {
		pid_t cc = fork();
		if (cc == 0) {
			dup2(cp[0], 0);
			close(cp[0]);
			close(cp[1]);
			char *av[] = { "cat", NULL };
			char *ev[] = { NULL };
			execve("/bin/cat", av, ev);
			_exit(127);
		}
		close(cp[0]);
		write(cp[1], "  cat<stdin: piped-stdin-ok\n", 28);
		close(cp[1]); /* last write end -> kernel EOFs the pipe, cat finishes */
		int ws;
		waitpid(cc, &ws, 0);
	}

	return 0;
}
