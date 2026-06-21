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
