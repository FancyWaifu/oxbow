/* Probe /lib/c.a on-disk size by reading it sequentially (only fopen/fread/
 * fclose, which tcc -run can resolve). If bytes << 1819746 the seed/write
 * truncated it — a large-file write or read bug that hits saving too. */
int printf(const char *, ...);
void *fopen(const char *, const char *);
unsigned long fread(void *, unsigned long, unsigned long, void *);
int fclose(void *);

int main(void) {
    void *f = fopen("/lib/c.a", "r");
    if (!f) {
        printf("CASIZE: cannot open /lib/c.a\n");
        return 1;
    }
    unsigned long total = 0, r;
    unsigned char buf[512];
    while ((r = fread(buf, 1, sizeof buf, f)) > 0) {
        total += r;
        if (total > 4000000)
            break;
    }
    printf("CASIZE: bytes=%lu (host c.a=1819746)\n", total);
    fclose(f);
    return 0;
}
