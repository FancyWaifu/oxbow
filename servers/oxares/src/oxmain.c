#include <stdio.h>
#include "ares.h"
int main(void) {
    printf("[cares-test] linked c-ares %s\n", ares_version(NULL));
    return 0;
}
