/* oxbow HAL for MicroPython: stdout/stdin over oxbow-libc fds (1/0). */
#include <unistd.h>
#include "py/mpconfig.h"

int mp_hal_stdin_rx_chr(void) {
    unsigned char c = 0;
    int r = read(0, &c, 1);
    if (r <= 0) {
        return -1;
    }
    return c;
}

mp_uint_t mp_hal_stdout_tx_strn(const char *str, mp_uint_t len) {
    if (len) {
        write(1, str, len);
    }
    return len;
}
