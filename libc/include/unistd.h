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
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4
int access(const char *, int);
ssize_t read(int, void *, size_t);
ssize_t write(int, const void *, size_t);
int close(int);
off_t lseek(int, off_t, int);
int ftruncate(int, off_t);
int unlink(const char *);
int execvp(const char *, char *const *);
char *getcwd(char *, size_t);
extern char **environ;
#include <sys/types.h>
uid_t getuid(void);
uid_t geteuid(void);
gid_t getgid(void);
gid_t getegid(void);
int getgroups(int, gid_t *);
char *getlogin(void);
int getlogin_r(char *, size_t);
#endif
