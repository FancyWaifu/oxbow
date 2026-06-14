#ifndef _SYS_STAT_H
#define _SYS_STAT_H
#include <sys/types.h>
struct stat {
    dev_t st_dev; ino_t st_ino; mode_t st_mode; nlink_t st_nlink;
    uid_t st_uid; gid_t st_gid; dev_t st_rdev; long st_size;
    long st_blksize; long st_blocks; long st_atime, st_mtime, st_ctime;
};
#define S_IFMT 0170000
#define S_IFREG 0100000
#define S_IFDIR 0040000
#define S_IFCHR 0020000
#define S_ISREG(m) (((m)&S_IFMT)==S_IFREG)
#define S_ISDIR(m) (((m)&S_IFMT)==S_IFDIR)
#define S_ISCHR(m) (((m)&S_IFMT)==S_IFCHR)
#define S_IRWXU 0700
#define S_IRUSR 0400
#define S_IWUSR 0200
int stat(const char *, struct stat *);
int fstat(int, struct stat *);
int mkdir(const char *, mode_t);
int chmod(const char *, mode_t);
mode_t umask(mode_t);
#endif
