#ifndef _STDIO_H
#define _STDIO_H
#include <stddef_shim.h>
#include <stdarg.h>
typedef struct FILE FILE;
extern FILE *stdin, *stdout, *stderr;
#define EOF (-1)
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#define BUFSIZ 1024
FILE *fopen(const char *, const char *);
FILE *fdopen(int, const char *);
int fclose(FILE *);
size_t fread(void *, size_t, size_t, FILE *);
size_t fwrite(const void *, size_t, size_t, FILE *);
char *fgets(char *, int, FILE *);
int fgetc(FILE *);
int getc(FILE *);
int ungetc(int, FILE *);
int fputs(const char *, FILE *);
int fputc(int, FILE *);
int putc(int, FILE *);
int putchar(int);
int puts(const char *);
int fseek(FILE *, long, int);
long ftell(FILE *);
int fflush(FILE *);
int feof(FILE *);
int ferror(FILE *);
void perror(const char *);
int remove(const char *);
FILE *freopen(const char *, const char *, FILE *);
int printf(const char *, ...);
int fprintf(FILE *, const char *, ...);
int sprintf(char *, const char *, ...);
int snprintf(char *, size_t, const char *, ...);
int vsnprintf(char *, size_t, const char *, va_list);
int vfprintf(FILE *, const char *, va_list);
int sscanf(const char *, const char *, ...);
#endif
