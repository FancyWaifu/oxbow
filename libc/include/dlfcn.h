#ifndef _DLFCN_H
#define _DLFCN_H
#define RTLD_NOW 2
#define RTLD_LAZY 1
#define RTLD_GLOBAL 0x100
#define RTLD_DEFAULT ((void*)0)
void *dlopen(const char *, int);
void *dlsym(void *, const char *);
int dlclose(void *);
char *dlerror(void);
#endif
