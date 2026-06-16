#ifndef _UNISTD_H
#define _UNISTD_H
#include <stddef_shim.h>
#ifndef SEEK_SET
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#endif
#define STDIN_FILENO 0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2
ssize_t read(int, void *, size_t);
ssize_t write(int, const void *, size_t);
int close(int);
off_t lseek(int, off_t, int);
int ftruncate(int, off_t);
int unlink(const char *);
int execvp(const char *, char *const *);
char *getcwd(char *, size_t);
extern char **environ;
#endif
