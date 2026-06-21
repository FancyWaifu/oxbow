/* POSIX getline()/getdelim() — oxbow-libc lacks them, and head/tail need getline.
 * Local to the sbase port for now; a natural future move into oxbow-libc proper. */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>

ssize_t
getdelim(char **lineptr, size_t *n, int delim, FILE *stream)
{
	size_t pos = 0;
	int c;

	if (!lineptr || !n || !stream) {
		errno = EINVAL;
		return -1;
	}
	if (*lineptr == NULL || *n == 0) {
		*n = 128;
		if (!(*lineptr = malloc(*n)))
			return -1;
	}
	for (;;) {
		c = getc(stream);
		if (c == EOF)
			break;
		if (pos + 1 >= *n) {
			size_t nn = *n * 2;
			char *np = realloc(*lineptr, nn);
			if (!np)
				return -1;
			*lineptr = np;
			*n = nn;
		}
		(*lineptr)[pos++] = (char)c;
		if (c == delim)
			break;
	}
	if (pos == 0 && c == EOF)
		return -1;
	(*lineptr)[pos] = '\0';
	return (ssize_t)pos;
}

ssize_t
getline(char **lineptr, size_t *n, FILE *stream)
{
	return getdelim(lineptr, n, '\n', stream);
}
