/* oxbow's entry into QuickJS: create a runtime + context, install a `print`
 * (and console.log), and evaluate a script — a built-in test, or a .js file
 * named in argv (read into memory via libc). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "quickjs.h"

static JSValue js_print(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    (void)this_val;
    for (int i = 0; i < argc; i++) {
        size_t len;
        const char *s = JS_ToCStringLen(ctx, &len, argv[i]);
        if (s) {
            if (i != 0) {
                fputc(' ', stdout);
            }
            fwrite(s, 1, len, stdout);
            JS_FreeCString(ctx, s);
        }
    }
    fputc('\n', stdout);
    return JS_UNDEFINED;
}

static void install_print(JSContext *ctx) {
    JSValue g = JS_GetGlobalObject(ctx);
    JSValue pr = JS_NewCFunction(ctx, js_print, "print", 1);
    JS_SetPropertyStr(ctx, g, "print", JS_DupValue(ctx, pr));
    // console.log = print
    JSValue console = JS_NewObject(ctx);
    JS_SetPropertyStr(ctx, console, "log", pr);
    JS_SetPropertyStr(ctx, g, "console", console);
    JS_FreeValue(ctx, g);
}

static void dump_exception(JSContext *ctx) {
    JSValue e = JS_GetException(ctx);
    const char *s = JS_ToCString(ctx, e);
    printf("Uncaught %s\n", s ? s : "(exception)");
    JS_FreeCString(ctx, s);
    // stack, if present
    if (JS_IsError(ctx, e)) {
        JSValue st = JS_GetPropertyStr(ctx, e, "stack");
        if (!JS_IsUndefined(st)) {
            const char *ss = JS_ToCString(ctx, st);
            if (ss) {
                printf("%s", ss);
                JS_FreeCString(ctx, ss);
            }
        }
        JS_FreeValue(ctx, st);
    }
    JS_FreeValue(ctx, e);
}

static const char *TEST =
    "print('QuickJS ' + (typeof globalThis) + ' running on oxbow!');\n"
    "let sq = Array.from({length: 10}, (_, i) => (i + 1) ** 2);\n"
    "print('squares 1..10:', JSON.stringify(sq));\n"
    "print('sum:', sq.reduce((a, b) => a + b, 0));\n"
    "const fib = n => n < 2 ? n : fib(n - 1) + fib(n - 2);\n"
    "print('fib(25) =', fib(25));\n"
    "let words = 'the quick brown fox the lazy dog the end'.split(' ');\n"
    "let counts = {};\n"
    "for (const w of words) counts[w] = (counts[w] || 0) + 1;\n"
    "print('word counts:', JSON.stringify(counts));\n"
    "print('pi =', Math.PI, ' sqrt2 =', Math.sqrt(2).toFixed(6));\n"
    "class Point { constructor(x, y) { this.x = x; this.y = y; }\n"
    "  dist() { return Math.hypot(this.x, this.y); }\n"
    "  toString() { return `Point(${this.x}, ${this.y})`; } }\n"
    "let p = new Point(3, 4);\n"
    "print('object:', p.toString(), ' dist:', p.dist());\n"
    "try { null.x; } catch (e) { print('caught:', e.constructor.name); }\n"
    "let re = /(\\w+)@(\\w+)/; let m = 'user@host'.match(re);\n"
    "print('regex:', m[1], m[2]);\n";

static char *read_file(const char *path) {
    FILE *f = fopen(path, "r");
    if (!f) {
        return NULL;
    }
    fseek(f, 0, 2);
    long n = ftell(f);
    fseek(f, 0, 0);
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
    JSRuntime *rt = JS_NewRuntime();
    if (!rt) {
        puts("qjs: cannot create runtime");
        return 1;
    }
    JSContext *ctx = JS_NewContext(rt);
    if (!ctx) {
        puts("qjs: cannot create context");
        return 1;
    }
    install_print(ctx);

    const char *src;
    const char *name;
    char *filebuf = NULL;
    if (argc > 1) {
        filebuf = read_file(argv[1]);
        if (!filebuf) {
            printf("qjs: cannot open '%s'\n", argv[1]);
            return 1;
        }
        src = filebuf;
        name = argv[1];
    } else {
        src = TEST;
        name = "<test>";
    }

    JSValue r = JS_Eval(ctx, src, strlen(src), name, JS_EVAL_TYPE_GLOBAL);
    int ret = 0;
    if (JS_IsException(r)) {
        dump_exception(ctx);
        ret = 1;
    }
    JS_FreeValue(ctx, r);
    if (filebuf) {
        free(filebuf);
    }

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
    return ret;
}
