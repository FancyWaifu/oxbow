/* First-light test program for the oxbow musl personality. Stock C against musl's
 * headers — printf goes musl stdio -> writev -> __oxbow_syscall -> oxbow tty. */
#include <stdio.h>

int
main(void)
{
	printf("Hello from musl libc, running on oxbow!\n");

	int sum = 0;
	for (int i = 1; i <= 10; i++)
		sum += i;
	printf("  sum(1..10) = %d via stock musl printf\n", sum);

	return 0;
}
