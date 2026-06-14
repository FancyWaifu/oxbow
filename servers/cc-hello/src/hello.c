/* A C program on oxbow, compiled by clang. It uses the fuller oxbow-libc:
 * argv, FILE* stdio (fopen/fgets/fprintf), <ctype.h>, <string.h>, malloc.
 * Behaves like `cat -n`: number the lines of a file. With no argument it reads
 * /etc/os-release; otherwise the path in argv[1] (relative to the shell's cwd). */
#include <stddef.h>

int printf(const char *, ...);
int puts(const char *);

typedef struct FILE FILE;
FILE *fopen(const char *, const char *);
int fclose(FILE *);
char *fgets(char *, int, FILE *);
int fprintf(FILE *, const char *, ...);
extern FILE *stdout;

int toupper(int);
unsigned long strlen(const char *);

void *mmap(void *, unsigned long, int, int, int, long);
int mprotect(void *, unsigned long, int);
#define PROT_READ 1
#define PROT_WRITE 2
#define PROT_EXEC 4
#define MAP_PRIVATE 2
#define MAP_ANON 0x20

/* The JIT primitive a real compiler (tcc -run) needs: write machine code into
 * RW memory, flip it to RX, and execute it — the W^X-compliant RW->RX transition
 * oxbow's sys_protect allows. We hand-assemble `int f(void){ return 42; }`. */
static void jit_demo(void) {
    unsigned char *code = (unsigned char *)mmap(
        0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (code == (void *)-1) { puts("  JIT: mmap failed"); return; }
    /* mov eax, 42 ; ret */
    code[0] = 0xb8; code[1] = 0x2a; code[2] = 0; code[3] = 0; code[4] = 0;
    code[5] = 0xc3;
    if (mprotect(code, 4096, PROT_READ | PROT_EXEC) != 0) {
        puts("  JIT: mprotect RW->RX failed");
        return;
    }
    int (*f)(void) = (int (*)(void))code;
    printf("  JIT: wrote code, flipped RW->RX, ran it -> %d (W^X transition!)\n", f());
}

int main(int argc, char **argv) {
    jit_demo();

    const char *path = (argc > 1) ? argv[1] : "etc/os-release";

    FILE *f = fopen(path, "r");
    if (!f) {
        printf("cc-hello: cannot open %s\n", path);
        return 1;
    }

    printf("cc-hello: numbering lines of %s (argc=%d)\n", path, argc);
    char line[128];
    int n = 0;
    while (fgets(line, sizeof line, f)) {
        n++;
        fprintf(stdout, "%4d  %s", n, line);
        /* if the line had no trailing newline, add one */
        unsigned long len = strlen(line);
        if (len == 0 || line[len - 1] != '\n') puts("");
    }
    fclose(f);
    printf("cc-hello: %d lines, uppercased first char of last = '%c'\n",
           n, toupper(line[0]));
    return 0;
}
