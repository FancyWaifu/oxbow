/* Exercise the oxbow-libc WRITE path (fopen/fwrite/fclose -> fsd -> ext2).
 * Run with `tcc -run writetest.c`, then `cat wout.txt` to confirm it persisted.
 * No #include: declare what we use, so tcc needs no headers on disk. */
int printf(const char *, ...);
void *fopen(const char *, const char *);
unsigned long fwrite(const void *, unsigned long, unsigned long, void *);
int fclose(void *);

int main(void) {
    void *f = fopen("wout.txt", "w");
    if (!f) {
        printf("WRITETEST: fopen failed\n");
        return 1;
    }
    const char *msg = "SAVED_VIA_LIBC_FWRITE\n";
    unsigned long n = fwrite(msg, 1, 22, f);
    printf("WRITETEST: fwrite returned %lu\n", n);
    fclose(f);
    printf("WRITETEST: done (cat wout.txt to verify)\n");
    return 0;
}
