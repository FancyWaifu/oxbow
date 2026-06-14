/* oxbow's entry into MicroPython: a GC heap, mp_init, and run a script — a
 * built-in test, or a .py file named in argv (read into memory via libc).
 * Adapted from ports/minimal/main.c (REPL stripped; file reading added). */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "py/builtin.h"
#include "py/compile.h"
#include "py/runtime.h"
#include "py/gc.h"
#include "py/mperrno.h"
#include "py/stackctrl.h"

#if MICROPY_ENABLE_COMPILER
static void do_str(const char *src, mp_parse_input_kind_t input_kind) {
    nlr_buf_t nlr;
    if (nlr_push(&nlr) == 0) {
        mp_lexer_t *lex = mp_lexer_new_from_str_len(MP_QSTR__lt_stdin_gt_, src, strlen(src), 0);
        qstr source_name = lex->source_name;
        mp_parse_tree_t parse_tree = mp_parse(lex, input_kind);
        mp_obj_t module_fun = mp_compile(&parse_tree, source_name, true);
        mp_call_function_0(module_fun);
        nlr_pop();
    } else {
        mp_obj_print_exception(&mp_plat_print, (mp_obj_t)nlr.ret_val);
    }
}
#endif

// The `open()` builtin needs a VFS/file-object layer oxbow doesn't have yet, so
// it raises. (io.StringIO/BytesIO still work; scripts run via `py <file>`.)
mp_obj_t mp_builtin_open(size_t n_args, const mp_obj_t *args, mp_map_t *kwargs) {
    (void)n_args;
    (void)args;
    (void)kwargs;
    mp_raise_OSError(MP_ENOENT);
}
MP_DEFINE_CONST_FUN_OBJ_KW(mp_builtin_open_obj, 1, mp_builtin_open);

static char *stack_top;
static char heap[MICROPY_HEAP_SIZE];

static const char *TEST =
    "print('MicroPython on oxbow!')\n"
    "nums = [x * x for x in range(1, 11)]\n"
    "print('squares 1..10:', nums)\n"
    "print('sum:', sum(nums))\n"
    "d = {}\n"
    "for w in 'the quick brown fox the lazy dog the end'.split():\n"
    "    d[w] = d.get(w, 0) + 1\n"
    "print('word counts:', d)\n"
    "def fib(n):\n"
    "    a, b = 0, 1\n"
    "    for _ in range(n):\n"
    "        a, b = b, a + b\n"
    "    return a\n"
    "print('fib(30) =', fib(30))\n"
    "class Point:\n"
    "    def __init__(self, x, y):\n"
    "        self.x, self.y = x, y\n"
    "    def __repr__(self):\n"
    "        return 'Point({}, {})'.format(self.x, self.y)\n"
    "print('object:', Point(3, 4))\n"
    "try:\n"
    "    raise ValueError('caught!')\n"
    "except ValueError as e:\n"
    "    print('exception:', e)\n";

/* Read an entire file into a malloc'd, NUL-terminated buffer. */
static char *read_file(const char *path) {
    FILE *f = fopen(path, "r");
    if (!f) {
        return NULL;
    }
    fseek(f, 0, 2); /* SEEK_END */
    long n = ftell(f);
    fseek(f, 0, 0); /* SEEK_SET */
    if (n < 0) {
        fclose(f);
        return NULL;
    }
    char *buf = (char *)malloc((size_t)n + 1);
    if (!buf) {
        fclose(f);
        return NULL;
    }
    size_t got = fread(buf, 1, (size_t)n, f);
    buf[got] = 0;
    fclose(f);
    return buf;
}

int main(int argc, char **argv) {
    int stack_dummy;
    stack_top = (char *)&stack_dummy;
    mp_stack_set_top(&stack_dummy);
    mp_stack_set_limit(64 * 1024);

    gc_init(heap, heap + sizeof(heap));
    mp_init();

    if (argc > 1) {
        char *src = read_file(argv[1]);
        if (src) {
            do_str(src, MP_PARSE_FILE_INPUT);
            free(src);
        } else {
            printf("micropython: cannot open '%s'\n", argv[1]);
        }
    } else {
        do_str(TEST, MP_PARSE_FILE_INPUT);
    }

    mp_deinit();
    return 0;
}

void gc_collect(void) {
    void *dummy;
    gc_collect_start();
    gc_collect_root(&dummy, ((mp_uint_t)stack_top - (mp_uint_t)&dummy) / sizeof(mp_uint_t));
    gc_collect_end();
}

mp_lexer_t *mp_lexer_new_from_file(qstr filename) {
    mp_raise_OSError(MP_ENOENT);
}

mp_import_stat_t mp_import_stat(const char *path) {
    (void)path;
    return MP_IMPORT_STAT_NO_EXIST;
}

void nlr_jump_fail(void *val) {
    (void)val;
    printf("micropython: FATAL nlr_jump_fail\n");
    for (;;) {
    }
}

void NORETURN __fatal_error(const char *msg) {
    printf("micropython: FATAL %s\n", msg);
    for (;;) {
    }
}

#ifndef NDEBUG
void MP_WEAK __assert_func(const char *file, int line, const char *func, const char *expr) {
    (void)func;
    printf("Assertion '%s' failed, at file %s:%d\n", expr, file, line);
    __fatal_error("Assertion failed");
}
#endif
