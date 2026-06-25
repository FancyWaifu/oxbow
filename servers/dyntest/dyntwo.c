/* §96 Phase 2: prove single-runtime symbol scope across the exe<->.so boundary.
 *
 * The exe defines `exe_add` over a static accumulator `g` and EXPORTS it into its
 * .dynsym via --dynamic-list (dyntwo.dynlist). The .so (acc.so) imports `exe_add`
 * and calls it from `accumulate`. Because ld-oxbow resolves exe-first, the .so's
 * `exe_add` binds to THIS function, and both sides bump the SAME `g`.
 *
 * _start drives it: two calls through the .so, then the exe reads `g` directly.
 * exit code = g = 15 proves (a) the exe->.so call works (accumulate, like Phase 1),
 * (b) the .so->exe call works (exe_add imported from the exe), and (c) they share
 * one runtime state. Raw oxbow syscalls, no libc. SYS_EXIT=7. */
extern int accumulate(int n); /* from /lib/acc.so */

static int g = 0;
/* Exported to the .so via --dynamic-list. Not `static` so it lands in .dynsym. */
int exe_add(int n) {
    g += n;
    return g;
}

static long sc(long nr, long a, long b, long c) {
    long ret, rdx_out;
    __asm__ volatile("syscall"
                     : "=a"(ret), "=d"(rdx_out)
                     : "a"(nr), "D"(a), "S"(b), "d"(c)
                     : "rcx", "r11", "memory");
    (void)rdx_out;
    return ret;
}

void _start(void) {
    accumulate(5);      /* .so -> exe_add(5)  -> g = 5  */
    accumulate(10);     /* .so -> exe_add(10) -> g = 15 */
    int t = exe_add(0); /* exe reads the SAME g via its own copy of exe_add */
    /* exit code = g: 15 proves the .so called back into the exe AND shares its state.
     * A separate-state bug would give 0/5/10; an unresolved callback would fault. */
    sc(7, t, 0, 0);
    for (;;) {
    }
}
