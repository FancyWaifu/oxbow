/* Byte-based UTF shim (see utf.h). */
#include <ctype.h>
#include <stdio.h>

#include "utf.h"

int
efgetrune(Rune *r, FILE *fp, const char *file)
{
	int ch = getc(fp);

	(void)file;
	if (ch == EOF) {
		*r = 0;
		return 0;
	}
	*r = ch;
	return 1; /* one byte == one rune in this shim */
}

int
isspacerune(Rune r)
{
	return isspace((int)r);
}

int
charntorune(Rune *r, const char *s, size_t len)
{
	if (len == 0) {
		*r = 0;
		return 0;
	}
	*r = (unsigned char)s[0]; /* byte == rune in this shim */
	return 1;
}
