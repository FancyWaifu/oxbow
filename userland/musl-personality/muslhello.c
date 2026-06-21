/* Test program for the oxbow musl personality. Stock C against musl's headers.
 * Phase 1: printf + exit. Phase 2: heap (malloc/free) + buffered stdio + real file
 * I/O over fsd — stat a file, read it via fopen/fgets, then create + write + read
 * back our own file. Everything below is unmodified upstream musl. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>

int
main(void)
{
	printf("Hello from musl libc, running on oxbow!\n");

	int sum = 0;
	for (int i = 1; i <= 10; i++)
		sum += i;
	printf("  sum(1..10) = %d via stock musl printf\n", sum);

	/* heap (mallocng over mmap) */
	char *buf = malloc(256);
	snprintf(buf, 256, "  malloc + snprintf at %p\n", (void *)buf);
	fputs(buf, stdout);
	free(buf);

	/* stat + buffered read of a seeded file */
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

	/* create + write + read back our own file */
	FILE *w = fopen("/musl-wrote.txt", "w");
	if (w) {
		fprintf(w, "written by musl libc on oxbow\n");
		fclose(w);
		FILE *r = fopen("/musl-wrote.txt", "r");
		if (r) {
			char back[64] = {0};
			fread(back, 1, sizeof back - 1, r);
			fclose(r);
			printf("  readback: %s", back);
		}
	}

	return 0;
}
