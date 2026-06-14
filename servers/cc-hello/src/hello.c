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

int main(int argc, char **argv) {
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
