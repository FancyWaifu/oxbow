/* Compiles on oxbow with real <...> system includes — the headers live on the
 * filesystem at /usr/include (oxbow-libc) and /usr/lib/tcc/include (tcc builtins).
 * Build + run on the device:  cc /inc.c -o /ic   then:  exec /ic   */
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    const char *msg = "hello via #include <stdio.h> on oxbow!";
    printf("%s (len=%lu)\n", msg, (unsigned long)strlen(msg));

    /* exercise malloc + a little stdlib */
    int *a = (int *)malloc(100 * sizeof(int));
    long sum = 0;
    for (int i = 0; i < 100; i++) {
        a[i] = i + 1;
        sum += a[i];
    }
    printf("  sum(1..100) = %ld, argc = %d\n", sum, argc);
    free(a);
    return 0;
}
