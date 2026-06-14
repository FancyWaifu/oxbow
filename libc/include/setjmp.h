#ifndef _SETJMP_H
#define _SETJMP_H
typedef long jmp_buf[8];
int setjmp(jmp_buf);
void longjmp(jmp_buf, int) __attribute__((noreturn));
#define sigsetjmp(b,s) setjmp(b)
#define siglongjmp(b,v) longjmp(b,v)
#endif
