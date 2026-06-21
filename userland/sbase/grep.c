/* oxbow lean grep — built on the vendored tiny-regex-c engine (re.c/re.h, public
 * domain) since oxbow-libc has no <regex.h>. NOT the verbatim sbase grep (which uses
 * POSIX regcomp + -E/-F/-w/-x/-o/-A/-B/-C). Supports the everyday flags:
 *     grep [-icnv] PATTERN [file ...]
 *   -i ignore case   -v invert   -n line numbers   -c count only
 * Regex subset (tiny-regex-c): ^ $ . * + ? [..] [^..] \d \w \s (no | or grouping).
 * Exit: 0 if any line matched, 1 if none, 2 on usage/error.
 */
#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "re.h"

static int iflag, vflag, nflag, cflag;

static void
lower_into(char *dst, const char *src, size_t n)
{
	size_t i;
	for (i = 0; i < n; i++)
		dst[i] = (char)tolower((unsigned char)src[i]);
	dst[n] = '\0';
}

static int
grep_stream(re_t re, FILE *fp, const char *name, int many)
{
	char *line = NULL, *low = NULL;
	size_t cap = 0, lowcap = 0;
	long lineno = 0, count = 0;
	long len;
	int any = 0;

	while ((len = getline(&line, &cap, fp)) > 0) {
		const char *hay = line;
		lineno++;
		if (iflag) {
			if ((size_t)len + 1 > lowcap) {
				lowcap = (size_t)len + 1;
				low = realloc(low, lowcap);
				if (!low)
					break;
			}
			lower_into(low, line, (size_t)len);
			hay = low;
		}
		int ml;
		int matched = re_matchp(re, hay, &ml) != -1;
		if (matched == !vflag) {
			any = 1;
			count++;
			if (!cflag) {
				if (many)
					printf("%s:", name);
				if (nflag)
					printf("%ld:", lineno);
				fputs(line, stdout);
				if (len == 0 || line[len - 1] != '\n')
					putchar('\n');
			}
		}
	}
	if (cflag) {
		if (many)
			printf("%s:", name);
		printf("%ld\n", count);
	}
	free(line);
	free(low);
	return any;
}

int
main(int argc, char *argv[])
{
	int i = 1, found = 0;
	const char *pat;
	char patbuf[256];
	re_t re;

	for (; i < argc && argv[i][0] == '-' && argv[i][1]; i++) {
		const char *f;
		for (f = argv[i] + 1; *f; f++) {
			switch (*f) {
			case 'i': iflag = 1; break;
			case 'v': vflag = 1; break;
			case 'n': nflag = 1; break;
			case 'c': cflag = 1; break;
			default:
				fprintf(stderr, "grep: unknown flag -%c\n", *f);
				return 2;
			}
		}
	}
	if (i >= argc) {
		fprintf(stderr, "usage: grep [-icnv] pattern [file ...]\n");
		return 2;
	}
	pat = argv[i++];
	if (iflag) {
		size_t pl = strlen(pat);
		if (pl >= sizeof patbuf)
			pl = sizeof patbuf - 1;
		lower_into(patbuf, pat, pl);
		pat = patbuf;
	}
	re = re_compile(pat);
	if (!re) {
		fprintf(stderr, "grep: bad pattern\n");
		return 2;
	}

	if (i >= argc) {
		found = grep_stream(re, stdin, "<stdin>", 0);
	} else {
		int many = (argc - i) > 1;
		for (; i < argc; i++) {
			FILE *fp = fopen(argv[i], "r");
			if (!fp) {
				fprintf(stderr, "grep: %s: cannot open\n", argv[i]);
				continue;
			}
			if (grep_stream(re, fp, argv[i], many))
				found = 1;
			fclose(fp);
		}
	}
	return found ? 0 : 1;
}
