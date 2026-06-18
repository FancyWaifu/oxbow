/* Controlled write-path test: write 100 KiB with a known byte pattern, then read
 * it back and verify. Run with `tcc -run writebig.c`. Isolates the fsd write
 * buffer from tcc compile-to-file AND from QEMU cross-reboot persistence (it all
 * happens in one boot). Expect: wrote 102400, read 102400, 0 mismatches. */
int printf(const char *, ...);
void *fopen(const char *, const char *);
unsigned long fwrite(const void *, unsigned long, unsigned long, void *);
unsigned long fread(void *, unsigned long, unsigned long, void *);
int fclose(void *);

#define CHUNKS 100
#define CHUNK 1024

int main(void) {
    unsigned char buf[CHUNK];
    for (int i = 0; i < CHUNK; i++)
        buf[i] = (unsigned char)i;

    void *f = fopen("/bigtest", "w");
    if (!f) {
        printf("BIG: open-w fail\n");
        return 1;
    }
    unsigned long wtot = 0;
    for (int k = 0; k < CHUNKS; k++)
        wtot += fwrite(buf, 1, CHUNK, f);
    fclose(f);
    printf("BIG: wrote %lu bytes\n", wtot);

    f = fopen("/bigtest", "r");
    if (!f) {
        printf("BIG: open-r fail\n");
        return 1;
    }
    unsigned long rtot = 0, bad = 0;
    unsigned char rb[CHUNK];
    for (int k = 0; k < CHUNKS; k++) {
        unsigned long n = fread(rb, 1, CHUNK, f);
        for (unsigned long j = 0; j < n; j++)
            if (rb[j] != (unsigned char)j)
                bad++;
        rtot += n;
    }
    fclose(f);
    printf("BIG: read %lu bytes, %lu mismatches\n", rtot, bad);
    printf("BIG: %s\n", (wtot == 102400 && rtot == 102400 && bad == 0) ? "PASS" : "FAIL");
    return 0;
}
