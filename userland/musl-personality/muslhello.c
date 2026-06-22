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

static volatile sig_atomic_t got_sig = 0;
static void on_usr1(int s) { got_sig = s; }

int
main(void)
{
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

	return 0;
}
