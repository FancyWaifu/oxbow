/* Output routing for the shell-embedded Lua. Force-included into every Lua .c
 * (build.rs -include) so the lua_writestring/lua_writeline/lua_writestringerror
 * macro overrides resolve to real declarations rather than implicit ones.
 *
 * Both functions ultimately forward to the Rust callback `ox_tty_write` (defined
 * in src/main.rs), which sends bytes to the console the shell owns (TAG_TTY_WRITE
 * on BOOT_TTY). The shell links libc with `default-features = false`, so libc's
 * stdout FILE is never initialized — Lua must not touch it. */
#ifndef OX_LUA_IO_H
#define OX_LUA_IO_H

#include <stddef.h>

/* Write `n` bytes of `s` to the shell's tty. */
void ox_lua_write(const char *s, size_t n);

/* Write a NUL-terminated error string (the lua_writestringerror format) to the
 * tty. The original macro takes a printf format + one arg; we drop the arg and
 * emit the format text — user-visible errors are surfaced by ox_lua_eval's own
 * pcall handling, so this only covers Lua's internal panic/trace path. */
void ox_lua_writeerr(const char *s);

#endif /* OX_LUA_IO_H */
