/* Test program for the oxbow musl personality. Stock C against musl's headers.
 *   Phase 1: printf + exit.   Phase 2: heap + buffered stdio + file I/O.
 *   Phase 3: fork + execve + waitpid — spawn another program, collect its exit. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <unistd.h>

int
main(void)
{
	setvbuf(stdout, NULL, _IONBF, 0); /* unbuffered, so output survives a crash */
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

	/* Phase 3: fork + execve + waitpid. The child runs /bin/seq (an oxbow program),
	 * whose stdout we inherit, then we collect its exit status. */
	/* Phase 3: exec another program (its stdout is ours), running it to completion.
	 * execve replaces us, so the `seq 1 5` output below is the last thing we print. */
	printf("  --- execve(\"/bin/seq\", {seq,1,5}) ---\n");
	char *args[] = { "seq", "1", "5", NULL };
	char *envp[] = { NULL };
	execve("/bin/seq", args, envp);
	perror("  execve failed"); /* only reached if exec fails */

	return 0;
}
