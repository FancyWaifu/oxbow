/* glue.c — oxbow-side glue for the weston port. The compositor entry (main) is in
 * oxbow-main.c; this file only carries the pixman arch get_implementations pass-throughs
 * (pixman is built generic-only — no SIMD — so the per-arch dispatchers resolve to no-ops). */

/* pixman built generic-only: the per-arch SIMD dispatchers are pass-throughs. */
typedef struct pixman_implementation_t pixman_implementation_t;
pixman_implementation_t *_pixman_arm_get_implementations(pixman_implementation_t *i) { return i; }
pixman_implementation_t *_pixman_mips_get_implementations(pixman_implementation_t *i) { return i; }
pixman_implementation_t *_pixman_ppc_get_implementations(pixman_implementation_t *i) { return i; }
pixman_implementation_t *_pixman_riscv_get_implementations(pixman_implementation_t *i) { return i; }
pixman_implementation_t *_pixman_x86_get_implementations(pixman_implementation_t *i) { return i; }
