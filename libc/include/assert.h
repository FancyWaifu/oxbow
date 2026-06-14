#ifndef _ASSERT_H
#define _ASSERT_H
void abort(void);
#define assert(x) ((x) ? (void)0 : abort())
#endif
