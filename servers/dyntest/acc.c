/* §96 Phase 2: a PIC shared library that calls BACK INTO the executable.
 *
 * `accumulate` leaves `exe_add` UNDEFINED — it is defined in the exe and exported
 * there via --dynamic-list. ld-oxbow resolves this .so's JUMP_SLOT for `exe_add`
 * against the exe's .dynsym (exe-first global scope), so the .so and the exe share
 * ONE copy of `exe_add` and its state. This is the "single runtime" proof: the
 * mechanism that lets a shared liboxui call back into the app's static libc. */
extern int exe_add(int n); /* defined in the exe, exported via --dynamic-list */

int accumulate(int n) { return exe_add(n); }
