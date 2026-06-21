/* POSIX regcomp/regexec shim for oxbow, backed by tiny-regex-c (re.c). Only the
 * match-or-not (REG_NOSUB) path is implemented — enough for nl. tiny-regex's
 * re_compile returns a single static buffer, so one pattern is live at a time. */
#include <regex.h>
#include <string.h>

#include "../re.h"

int
regcomp(regex_t *preg, const char *pattern, int cflags)
{
	(void)cflags;
	preg->prog = (void *)re_compile(pattern);
	return preg->prog ? 0 : 1;
}

int
regexec(const regex_t *preg, const char *string, size_t nmatch,
        regmatch_t *pmatch, int eflags)
{
	int len;

	(void)nmatch;
	(void)pmatch;
	(void)eflags;
	if (!preg->prog)
		return REG_NOMATCH;
	return re_matchp((re_t)preg->prog, string, &len) >= 0 ? 0 : REG_NOMATCH;
}

size_t
regerror(int errcode, const regex_t *preg, char *errbuf, size_t errbuf_size)
{
	const char *m = "invalid regex";
	size_t n = strlen(m);

	(void)errcode;
	(void)preg;
	if (errbuf_size) {
		size_t k = n < errbuf_size - 1 ? n : errbuf_size - 1;
		memcpy(errbuf, m, k);
		errbuf[k] = '\0';
	}
	return n + 1;
}

void
regfree(regex_t *preg)
{
	preg->prog = NULL;
}
