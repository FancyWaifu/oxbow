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
 *   [0]=argv0  [1]=NULL(argv end)  [2]=NULL(envp end, empty env)
 *   [3]=AT_RANDOM [4]=&rnd  [5]=AT_PAGESZ [6]=4096  [7]=AT_NULL [8]=0 */
static long block[16];
static char arg0[] = "musl";
static unsigned char rnd[16];

#define AT_NULL    0
#define AT_PAGESZ  6
#define AT_RANDOM 25

__attribute__((noreturn)) void oxbow_main(void)
{
	/* 16 real random bytes for musl's stack-guard / malloc seed. */
	ox_syscall2(OX_SYS_GETENTROPY, (long)rnd, (long)sizeof rnd);

	block[0] = (long)arg0; /* argv[0]            */
	block[1] = 0;          /* argv terminator    */
	block[2] = 0;          /* envp[0] = NULL      */
	block[3] = AT_RANDOM;
	block[4] = (long)rnd;
	block[5] = AT_PAGESZ;
	block[6] = 4096;
	block[7] = AT_NULL;
	block[8] = 0;

	__libc_start_main(main, 1, (char **)block);

	/* __libc_start_main exits via exit(); never returns. Belt-and-braces: */
	ox_syscall1(OX_SYS_EXIT, 0);
	__builtin_unreachable();
}
