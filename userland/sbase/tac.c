/* tac — concatenate and print files with lines in reverse order.
 * Lean oxbow original (sbase has no tac); uses the libutil surface only. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "util.h"

static char **lines = NULL;
static size_t nlines = 0, alloc = 0;

static void
tac(FILE *fp)
{
	char *line = NULL;
	size_t size = 0;
	ssize_t n;

	while ((n = getline(&line, &size, fp)) > 0) {
		if (nlines == alloc) {
			alloc = alloc ? alloc * 2 : 64;
			lines = ereallocarray(lines, alloc, sizeof *lines);
		}
		lines[nlines] = emalloc(n + 1);
		memcpy(lines[nlines], line, n);
		lines[nlines][n] = '\0';
		nlines++;
	}
	free(line);

	while (nlines--) {
		char *l = lines[nlines];
		size_t len = strlen(l);
		int hadlf = len && l[len - 1] == '\n';

		if (hadlf)
			l[len - 1] = '\0';
		fputs(l, stdout);
		fputc('\n', stdout);
		free(l);
	}
	nlines = 0;
	alloc = 0;
	free(lines);
	lines = NULL;
}

int
main(int argc, char *argv[])
{
	FILE *fp;
	int ret = 0;

	argv0 = argv[0], argc--, argv++;

	if (!argc) {
		tac(stdin);
	} else {
		for (; *argv; argc--, argv++) {
			if (!strcmp(*argv, "-")) {
				fp = stdin;
			} else if (!(fp = fopen(*argv, "r"))) {
				weprintf("fopen %s:", *argv);
				ret = 1;
				continue;
			}
			tac(fp);
			if (fp != stdin)
				fshut(fp, *argv);
		}
	}

	ret |= fshut(stdout, "<stdout>");
	return ret;
}
