/* oxbow personality override of musl's x86_64 vfork.
 *
 * Upstream issues a RAW `vfork` syscall (eax=58) and stashes the return address in
 * %rdx across it. That bypasses the personality's syscall_arch.h C override, AND Linux
 * nr 58 (vfork) collides with oxbow's SYS_YIELD — whose 2nd return value clobbers %rdx,
 * destroying the saved return address, so the function returns to a garbage rip (a
 * #PF at rip=0). oxbow has no vfork primitive.
 *
 * Route vfork() to the real fork() instead (a separate-address-space clone, via the C
 * override -> the personality's fork handler). The vfork *contract* — the child only
 * execs or _exit()s and never returns into the caller — holds for the one user that
 * matters here (dash's vforkexec), so a separate-AS fork is a correct, safe substitute.
 */
.text
.global vfork
.type vfork,@function
vfork:
	jmp fork
