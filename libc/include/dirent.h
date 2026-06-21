/* Minimal POSIX dirent for oxbow-libc — directory streams over the fs server
 * (opendir/readdir/closedir; rt::fs::readdir backs them). Enough for a recursive
 * directory walk (find, ls). No rewinddir/seekdir/scandir yet. */
#ifndef _DIRENT_H
#define _DIRENT_H

struct dirent {
	unsigned long  d_ino;
	long           d_off;
	unsigned short d_reclen;
	unsigned char  d_type;
	char           d_name[256];
};

/* d_type values */
#define DT_UNKNOWN 0
#define DT_DIR     4
#define DT_REG     8

typedef struct __oxdir DIR;

DIR           *opendir(const char *);
struct dirent *readdir(DIR *);
int            closedir(DIR *);

#endif
