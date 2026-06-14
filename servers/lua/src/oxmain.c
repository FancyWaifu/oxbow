/* oxbow's entry into Lua: create a state, open the libraries that work on oxbow
 * (no file I/O / os / dynamic loading yet), and run a test script that exercises
 * the interpreter. This replaces lua.c's standalone REPL/main. */
#include "lua.h"
#include "lualib.h"
#include "lauxlib.h"

/* A reduced set of standard libraries — the ones that don't need a filesystem,
 * a clock, or dlopen. The number operators (// % ^) live in the core VM and only
 * need floor/fmod/pow from libc, so they work without the math *library*. */
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

static const char *TEST =
    "print(_VERSION .. ' running on oxbow!')\n"
    "local t = {}\n"
    "for i = 1, 10 do t[i] = i * i end\n"
    "local s = 0\n"
    "for _, v in ipairs(t) do s = s + v end\n"
    "print('sum of squares 1..10 = ' .. s)\n"
    "print(string.format('7/2=%.4f  7//2=%d  7%%3=%d  2^10=%d', 7/2, 7//2, 7%3, 2^10))\n"
    "local function fib(n) if n < 2 then return n else return fib(n-1) + fib(n-2) end end\n"
    "print('fib(25) = ' .. fib(25))\n"
    "print('upper: ' .. string.upper('hello from a real language on oxbow'))\n"
    "local rev = {}\n"
    "for w in string.gmatch('one two three four', '%a+') do rev[#rev+1] = w end\n"
    "print('reversed words: ' .. table.concat({rev[4],rev[3],rev[2],rev[1]}, ' '))\n";

int main(int argc, char **argv) {
    lua_State *L = luaL_newstate();
    if (L == NULL) {
        puts("lua: cannot create state (out of memory?)");
        return 1;
    }
    ox_openlibs(L);

    /* If given an argument, run that .lua file; otherwise the built-in test. */
    int status;
    if (argc > 1) {
        status = luaL_dofile(L, argv[1]);
    } else {
        status = luaL_dostring(L, TEST);
    }
    if (status != LUA_OK) {
        const char *msg = lua_tostring(L, -1);
        printf("lua: error: %s\n", msg ? msg : "(unknown)");
        lua_close(L);
        return 1;
    }
    lua_close(L);
    return 0;
}
