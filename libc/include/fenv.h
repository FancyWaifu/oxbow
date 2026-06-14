#ifndef _FENV_H
#define _FENV_H
/* oxbow: no FP exception/rounding control; stubs so code that includes <fenv.h>
 * compiles. Default-rounding only. */
#define FE_TONEAREST 0
#define FE_DOWNWARD 1
#define FE_UPWARD 2
#define FE_TOWARDZERO 3
static inline int fesetround(int r) { (void)r; return 0; }
static inline int fegetround(void) { return FE_TONEAREST; }
#endif
