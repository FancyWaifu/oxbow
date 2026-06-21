/* oxbow lean find — a compact recursive find on oxbow-libc's dirent + stat.
 *
 * This is NOT the verbatim sbase find (1100 lines): that one needs openat/fstatat/
 * dirfd, fnmatch, getpwnam/getgrnam, and sys/wait for -exec — a lot of libc surface
 * oxbow doesn't have yet. This covers the common case:
 *     find [path...] [-name PATTERN] [-type f|d] [-print]
 * Default path is ".", default action prints each match. -name uses a small glob
 * (* and ?). Richer predicates (-exec/-user/-newer/-regex) are out of scope for now.
 */
#include <dirent.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>

static const char *gname;  /* -name pattern, or NULL */
static char        gtype;  /* 'f' or 'd', or 0 for any */

/* Small glob: '*' (any run) and '?' (one char); everything else literal. Enough
 * for -name on a single path component. */
static int
globmatch(const char *p, const char *s)
{
	for (; *p; p++, s++) {
		if (*p == '*') {
			p++;
			if (!*p)
				return 1;
			for (; *s; s++)
				if (globmatch(p, s))
					return 1;
			return globmatch(p, s);
		} else if (*p == '?') {
			if (!*s)
				return 0;
		} else {
			if (*p != *s)
				return 0;
		}
	}
	return *s == 0;
}

static const char *
base_of(const char *path)
{
	const char *b = strrchr(path, '/');
	return b ? b + 1 : path;
}

static void
report(const char *path, int is_dir)
{
	if (gtype == 'f' && is_dir)
		return;
	if (gtype == 'd' && !is_dir)
		return;
	if (gname && !globmatch(gname, base_of(path)))
		return;
	printf("%s\n", path);
}

static void
walk(const char *path)
{
	struct stat st;
	struct dirent *e;
	DIR *d;
	int is_dir;
	char child[1024];

	if (stat(path, &st) < 0) {
		fprintf(stderr, "find: %s: cannot stat\n", path);
		return;
	}
	is_dir = S_ISDIR(st.st_mode);
	report(path, is_dir);
	if (!is_dir)
		return;

	if (!(d = opendir(path)))
		return;
	while ((e = readdir(d))) {
		if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, ".."))
			continue;
		if (!strcmp(path, "/"))
			snprintf(child, sizeof child, "/%s", e->d_name);
		else
			snprintf(child, sizeof child, "%s/%s", path, e->d_name);
		walk(child);
	}
	closedir(d);
}

int
main(int argc, char *argv[])
{
	const char *paths[64];
	int np = 0, i = 1;

	/* Leading non-flag args are the search roots. */
	for (; i < argc && argv[i][0] != '-'; i++)
		if (np < 64)
			paths[np++] = argv[i];
	for (; i < argc; i++) {
		if (!strcmp(argv[i], "-name") && i + 1 < argc)
			gname = argv[++i];
		else if (!strcmp(argv[i], "-type") && i + 1 < argc)
			gtype = argv[++i][0];
		else if (!strcmp(argv[i], "-print"))
			; /* default action */
		else {
			fprintf(stderr, "find: unsupported argument: %s\n", argv[i]);
			return 1;
		}
	}
	if (np == 0)
		paths[np++] = ".";
	for (i = 0; i < np; i++)
		walk(paths[i]);
	return 0;
}
