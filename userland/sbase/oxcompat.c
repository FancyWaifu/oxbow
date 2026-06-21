/* Small libc gaps oxbow-libc doesn't provide, needed by the sbase tools (tail). */

long long
llabs(long long x)
{
	return x < 0 ? -x : x;
}

/* Real-ish sleep: oxbow-libc has no sleep(), so spin on the boot clock. Used only
 * by `tail -f` (a niche, interactive path); a yield-based version is a future nicety. */
extern unsigned long long ox_uptime_ms(void);

unsigned int
sleep(unsigned int sec)
{
	unsigned long long start = ox_uptime_ms();
	unsigned long long want = (unsigned long long)sec * 1000ULL;

	while (ox_uptime_ms() - start < want)
		;
	return 0;
}

/* libc gaps for the wider coreutils batch. */
#include <stddef.h>
char *
strndup(const char *s, size_t n)
{
	extern void *malloc(size_t);
	size_t i = 0;
	while (i < n && s[i])
		i++;
	char *p = (char *)malloc(i + 1);
	if (p) {
		for (size_t k = 0; k < i; k++)
			p[k] = s[k];
		p[i] = 0;
	}
	return p;
}

struct timespec { long tv_sec; long tv_nsec; };
int
nanosleep(const struct timespec *req, struct timespec *rem)
{
	extern unsigned long long ox_uptime_ms(void);
	(void)rem;
	if (!req)
		return -1;
	unsigned long long ms = (unsigned long long)req->tv_sec * 1000ULL + (unsigned long long)req->tv_nsec / 1000000ULL;
	unsigned long long start = ox_uptime_ms();
	while (ox_uptime_ms() - start < ms)
		;
	return 0;
}

void *
memmem(const void *h, size_t hl, const void *n, size_t nl)
{
	const unsigned char *hp = (const unsigned char *)h;
	const unsigned char *np = (const unsigned char *)n;
	if (nl == 0)
		return (void *)hp;
	if (hl < nl)
		return NULL;
	for (size_t i = 0; i + nl <= hl; i++) {
		size_t j = 0;
		while (j < nl && hp[i + j] == np[j])
			j++;
		if (j == nl)
			return (void *)(hp + i);
	}
	return NULL;
}

int isblank(int c) { return c == ' ' || c == '\t'; }

void *
bsearch(const void *key, const void *base, size_t n, size_t sz,
        int (*cmp)(const void *, const void *))
{
	size_t lo = 0, hi = n;
	while (lo < hi) {
		size_t mid = lo + (hi - lo) / 2;
		const void *p = (const char *)base + mid * sz;
		int r = cmp(key, p);
		if (r < 0)
			hi = mid;
		else if (r > 0)
			lo = mid + 1;
		else
			return (void *)p;
	}
	return NULL;
}

/* libgen: dirname/basename for the dirname/basename tools. Operate in place on
 * the passed buffer (POSIX permits modifying the argument). */
char *
dirname(char *path)
{
	if (!path || !*path)
		return (char *)".";
	size_t n = 0;
	while (path[n])
		n++;
	while (n > 1 && path[n - 1] == '/')
		path[--n] = 0;
	char *slash = NULL;
	for (size_t i = 0; i < n; i++)
		if (path[i] == '/')
			slash = &path[i];
	if (!slash)
		return (char *)".";
	if (slash == path)
		return (char *)"/";
	*slash = 0;
	return path;
}

int isprint(int c) { return c >= 0x20 && c < 0x7f; }
