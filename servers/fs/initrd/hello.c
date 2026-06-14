/* A C program to compile + run ON oxbow with `tcc -run hello.c`. No #include —
 * it declares what it uses, so tcc needs no header files on disk. tcc JITs this,
 * resolves printf against the running oxbow-libc (via dlsym), and runs main(). */
int printf(const char *, ...);

int main(void) {
    printf("Hello from C, compiled AND run on oxbow by tcc -run!\n");
    int sum = 0;
    for (int i = 1; i <= 10; i++) sum += i;
    printf("  the JIT-compiled loop says sum(1..10) = %d\n", sum);
    return 0;
}
