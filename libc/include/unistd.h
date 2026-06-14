#ifndef _UNISTD_H
#define _UNISTD_H
#include <stddef_shim.h>
ssize_t read(int, void *, size_t);
ssize_t write(int, const void *, size_t);
int close(int);
off_t lseek(int, off_t, int);
int unlink(const char *);
int execvp(const char *, char *const *);
char *getcwd(char *, size_t);
extern char **environ;
#endif
