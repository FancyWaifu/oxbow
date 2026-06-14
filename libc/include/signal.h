#ifndef _SIGNAL_H
#define _SIGNAL_H
typedef int sig_atomic_t;
typedef void (*sighandler_t)(int);
#define SIGABRT 6
#define SIGSEGV 11
#define SIGFPE  8
#define SIG_DFL ((sighandler_t)0)
sighandler_t signal(int, sighandler_t);
int raise(int);
#endif
