/* luaglue.c — the bridge between the Rust shell and the embedded Lua 5.4 VM.
 *
 * Responsibilities:
 *   - create a Lua state with the libraries that work on oxbow (no fs/os/dlopen),
 *   - evaluate one line of Lua with interactive-REPL semantics (a bare expression
 *     like `1+2` prints its value; statements like `for ... end` just run),
 *   - route ALL Lua output + errors to the shell's tty (ox_tty_write), since the
 *     shell links libc without the stdout FILE that Lua's print would otherwise use.
 *
 * This replaces lua.c's standalone main/REPL and linit.c's openlibs. The Rust
 * side (src/main.rs) calls ox_lua_new() once, then ox_lua_eval() per line. */
#include <string.h>

#include "lua.h"
#include "lualib.h"
#include "lauxlib.h"
#include "ox_lua_io.h"

/* Implemented in Rust (src/main.rs): write `n` bytes to the shell's console. */
extern void ox_tty_write(const unsigned char *p, size_t n);

/* Implemented in Rust (src/main.rs): the Lua<->shell bridge (§83).
 * ox_shell_run runs a command line (output to the tty) and returns its exit
 * status; ox_shell_capture runs it and writes its stdout into `out` (cap bytes),
 * returning the length. Both use the shell's current Spawner + cwd. */
extern int ox_shell_run(const char *s, size_t n);
extern int ox_shell_capture(const char *s, size_t n, char *out, int cap);

/* Lua: sh("cmd") -> run a command line, return its exit status (0 = success). */
static int ox_sh(lua_State *L) {
    size_t n;
    const char *s = luaL_checklstring(L, 1, &n);
    lua_pushinteger(L, ox_shell_run(s, n));
    return 1;
}

/* Lua: sh_out("cmd") -> run a command, return its stdout as a string (trailing
 * newline trimmed). Output is bounded by the capture buffer below. */
static int ox_sh_out(lua_State *L) {
    size_t n;
    const char *s = luaL_checklstring(L, 1, &n);
    char buf[2048];
    int len = ox_shell_capture(s, n, buf, (int)sizeof(buf));
    if (len < 0) len = 0;
    while (len > 0 && buf[len - 1] == '\n') len--; /* trim trailing newline */
    lua_pushlstring(L, buf, (size_t)len);
    return 1;
}

void ox_lua_write(const char *s, size_t n) {
    if (s && n) ox_tty_write((const unsigned char *)s, n);
}

void ox_lua_writeerr(const char *s) {
    if (s) ox_tty_write((const unsigned char *)s, strlen(s));
}

/* The reduced standard library set — no filesystem, clock, or dynamic loading.
 * The // % ^ operators live in the core VM (only need floor/fmod/pow from libc),
 * so no math *library* is required. Mirrors servers/lua/src/oxmain.c. */
static const luaL_Reg oxlibs[] = {
    {LUA_GNAME, luaopen_base},
    {LUA_TABLIBNAME, luaopen_table},
    {LUA_STRLIBNAME, luaopen_string},
    {LUA_COLIBNAME, luaopen_coroutine},
    {LUA_UTF8LIBNAME, luaopen_utf8},
    {NULL, NULL},
};

static void ox_openlibs(lua_State *L) {
    const luaL_Reg *lib;
    for (lib = oxlibs; lib->func; lib++) {
        luaL_requiref(L, lib->name, lib->func, 1);
        lua_pop(L, 1);
    }
}

/* Last-resort panic (an error escaping a pcall — shouldn't happen, since
 * ox_lua_eval runs everything protected). Report it through the tty rather than
 * touching libc stderr. */
static int ox_panic(lua_State *L) {
    const char *msg = lua_tostring(L, -1);
    ox_lua_write("lua panic: ", 11);
    if (msg) ox_lua_write(msg, strlen(msg));
    ox_lua_write("\n", 1);
    return 0;
}

/* Write the string value of Lua global `name` into `out` (capacity `cap`,
 * NUL-terminated). Returns the length (excluding NUL), or -1 if the global is
 * nil/undefined. Backs the shell's `$VAR` expansion — a shell "variable" IS a Lua
 * global, so `x = 5` then `echo $x` prints 5. Numbers/booleans stringify too. */
int ox_lua_global(lua_State *L, const char *name, char *out, int cap) {
    lua_getglobal(L, name);
    if (lua_isnil(L, -1)) {
        lua_pop(L, 1);
        return -1;
    }
    size_t len;
    const char *s = luaL_tolstring(L, -1, &len); /* pushes a string form */
    int n = (int)len;
    if (n > cap - 1) n = cap - 1;
    if (n < 0) n = 0;
    for (int i = 0; i < n; i++) out[i] = s[i];
    out[n] = '\0';
    lua_pop(L, 2); /* the tolstring result + the original global */
    return n;
}

/* Create the shell's persistent Lua state. Returns NULL on OOM. */
lua_State *ox_lua_new(void) {
    lua_State *L = luaL_newstate();
    if (L == NULL) return NULL;
    lua_atpanic(L, ox_panic);
    ox_openlibs(L);
    // §83: the Lua<->shell bridge — run/capture shell commands from Lua.
    lua_register(L, "sh", ox_sh);
    lua_register(L, "sh_out", ox_sh_out);
    return L;
}

static void report_top_error(lua_State *L) {
    const char *msg = lua_tostring(L, -1);
    if (msg) {
        ox_lua_write(msg, strlen(msg));
        ox_lua_write("\n", 1);
    }
}

/* Evaluate one line of Lua against the persistent state, REPL-style.
 *
 * First try the line as an expression by compiling "return <line>": that makes a
 * bare `1+2` evaluate to a value we then print (via the global `print`, so the
 * output routing macros apply). If that fails to compile, run the line verbatim
 * as a statement (assignments, control flow, function defs). Globals persist
 * across calls; chunk-local `local`s do not (each line is its own chunk).
 *
 * Returns LUA_OK on success, or the Lua error status (message already printed). */
int ox_lua_eval(lua_State *L, const char *code) {
    int base = lua_gettop(L);
    int is_expr = 0;
    int status;
    char buf[1024];
    size_t len = strlen(code);

    if (len + 8 < sizeof(buf)) {
        memcpy(buf, "return ", 7);
        memcpy(buf + 7, code, len);
        buf[7 + len] = '\0';
        if (luaL_loadstring(L, buf) == LUA_OK) {
            is_expr = 1;
        } else {
            lua_pop(L, 1); /* discard the expression-form syntax error */
        }
    }
    if (!is_expr) {
        if (luaL_loadstring(L, code) != LUA_OK) {
            report_top_error(L);
            lua_settop(L, base);
            return LUA_ERRSYNTAX;
        }
    }

    status = lua_pcall(L, 0, is_expr ? LUA_MULTRET : 0, 0);
    if (status != LUA_OK) {
        report_top_error(L);
        lua_settop(L, base);
        return status;
    }

    if (is_expr) {
        int n = lua_gettop(L) - base;
        if (n > 0) {
            luaL_checkstack(L, n + 1, "too many results to print");
            lua_getglobal(L, "print");  /* push print above the results */
            lua_insert(L, base + 1);    /* move it below them */
            if (lua_pcall(L, n, 0, 0) != LUA_OK) {
                report_top_error(L);
            }
        }
    }

    lua_settop(L, base);
    return LUA_OK;
}
