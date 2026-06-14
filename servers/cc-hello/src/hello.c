/* A real C program, compiled by clang, running on oxbow. It includes no system
 * headers (none exist yet) — it just declares the libc functions it calls,
 * which the Rust side provides over oxbow's capability syscalls. */
int puts(const char *s);
int printf(const char *fmt, ...);
void *malloc(unsigned long);
void free(void *);
unsigned long strlen(const char *);
int open(const char *path, int flags);
long read(int fd, void *buf, unsigned long len);
long write(int fd, const void *buf, unsigned long len);
int close(int fd);

int main(int argc, char **argv) {
    (void)argc; (void)argv;
    puts("hello from C, compiled by clang, running on oxbow");
    printf("  2 + 2 = %d, and %s works too\n", 2 + 2, "printf");

    /* exercise the heap: malloc, build a string, print it, free it */
    char *p = (char *)malloc(64);
    for (int i = 0; i < 6; i++) p[i] = 'A' + i;
    p[6] = 0;
    printf("  malloc'd \"%s\" (strlen %d) on oxbow's slab heap\n", p, (int)strlen(p));
    free(p);

    /* POSIX file I/O over capabilities: open a file (relative to cwd),
     * read it in a loop, write it to stdout — a tiny C cat. */
    int fd = open("etc/os-release", 0);
    if (fd < 0) {
        puts("  (open etc/os-release failed — run me from /)");
        return 0;
    }
    puts("  open()/read()/write() etc/os-release via the fs capability:");
    char buf[64];
    long n;
    while ((n = read(fd, buf, sizeof buf)) > 0) write(1, buf, n);
    close(fd);
    return 0;
}
