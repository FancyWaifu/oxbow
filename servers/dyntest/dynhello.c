/* §96 dyntest: a dynamically-linked executable. It calls add() from libadd.so
 * (resolved by ld-oxbow), then writes the result to the console (serial) and exits.
 * Raw oxbow syscalls — no libc. SYS_CONSOLE_WRITE=6, SYS_EXIT=7; BOOT_CONSOLE=2. */
extern int add(int, int);

static long sc(long nr, long a, long b, long c) {
    long ret, rdx_out; /* oxbow returns two values (rax,rdx); capture rdx as clobbered */
    __asm__ volatile("syscall"
        : "=a"(ret), "=d"(rdx_out)
        : "a"(nr), "D"(a), "S"(b), "d"(c)
        : "rcx", "r11", "memory");
    (void)rdx_out;
    return ret;
}

void _start(void) {
    int r = add(3, 4);
    static char msg[] = "ld-oxbow OK: 3+4=_\n";
    msg[17] = (char)('0' + r); /* '_' -> '7' if linking worked */
    sc(6, 2, (long)msg, sizeof(msg) - 1);
    sc(7, 0, 0, 0);
    for (;;) { }
}
