/* crt bridge: oxbow entry -> musl __libc_start_main.
 *
 * oxbow-rt's _start enters here as oxbow_main() with just a fresh stack — there is
 * no Linux-style initial stack (argc/argv/envp/auxv). musl's __libc_start_main +
 * __init_libc expect that layout: argv, then a NULL, then envp, then a NULL, then
 * the auxv, contiguous in memory (__init_libc walks past envp's NULL to find auxv,
 * and __init_ssp reads 16 bytes from AT_RANDOM). We synthesize a minimal one.
 *
 * Link order: oxbow-rt (_start + IPC shims) + this + oxbow_syscall.o + musl libc.a. */
#include "oxsys.h"

extern int __libc_start_main(int (*)(int, char **, char **), int, char **);
extern int main(int, char **, char **);

/* The synthetic argv/envp/auxv block (contiguous, as __init_libc requires):
 *   argv[0..argc-1], NULL (argv end), NULL (envp end, empty env), then the auxv
 *   (AT_RANDOM, AT_PAGESZ, AT_NULL). argv is taken from oxbow's SPAWN_ARGV page (a
 *   NUL-terminated, space-separated string the kernel maps on spawn), split on
 *   spaces — so `awk -f /prog.awk /input` reaches main() as real argv. (oxbow does
 *   not preserve arg boundaries across spaces, so a single argument cannot contain a
 *   space; pass a program-with-spaces via a file, not an inline quoted token.) */
/* oxbow's SPAWN_ARGV holds only the arguments AFTER the command name (the shell
 * strips the verb), but C wants argv[0] = the program name. So we always synthesize
 * argv[0] here (OXBOW_ARGV0, set per-program by build.rs) and place the SPAWN_ARGV
 * tokens at argv[1..]. */
#ifndef OXBOW_ARGV0
#define OXBOW_ARGV0 "prog"
#endif
#define SPAWN_ARGV 0x0F000000UL
#define MAXARGV 96
static long block[MAXARGV + 8];
static char argbuf[4096];
static char arg0[] = OXBOW_ARGV0;
static unsigned char rnd[16];

#define AT_NULL    0
#define AT_PAGESZ  6
#define AT_RANDOM 25

extern void __oxbow_register_sigdispatch(void);

__attribute__((noreturn)) void oxbow_main(void)
{
	/* 16 real random bytes for musl's stack-guard / malloc seed. */
	ox_syscall2(OX_SYS_GETENTROPY, (long)rnd, (long)sizeof rnd);

	/* Phase 9 step 2: register the async-signal dispatcher so a Ctrl-C delivered to
	 * us while running (not at a read) runs our SIGINT handler instead of a kill. */
	__oxbow_register_sigdispatch();

	/* Copy the SPAWN_ARGV string into our own buffer, then split it in place. */
	const char *src = (const char *)SPAWN_ARGV;
	int n = 0;
	while (n < (int)sizeof argbuf - 1 && src[n])
		argbuf[n] = src[n], n++;
	argbuf[n] = 0;

	block[0] = (long)arg0; /* synthesized argv[0] = program name */
	int argc = 1, i = 0;
	while (i < n && argc < MAXARGV) {
		while (i < n && argbuf[i] == ' ')
			i++; /* skip run of spaces */
		if (i >= n)
			break;
		block[argc++] = (long)&argbuf[i]; /* token start */
		while (i < n && argbuf[i] != ' ')
			i++;
		if (i < n)
			argbuf[i++] = 0; /* NUL-terminate this token */
	}

	block[argc] = 0;            /* argv terminator     */
	block[argc + 1] = 0;        /* envp[0] = NULL       */
	block[argc + 2] = AT_RANDOM;
	block[argc + 3] = (long)rnd;
	block[argc + 4] = AT_PAGESZ;
	block[argc + 5] = 4096;
	block[argc + 6] = AT_NULL;
	block[argc + 7] = 0;

	__libc_start_main(main, argc, (char **)block);

	/* __libc_start_main exits via exit(); never returns. Belt-and-braces: */
	ox_syscall1(OX_SYS_EXIT, 0);
	__builtin_unreachable();
}
