//! shell — the interactive command line (module 4). Each iteration: print the
//! `oxbow$ ` prompt, read a line from the tty, parse it, and run a builtin.
//! All output is routed back THROUGH the tty (TAG_TTY_WRITE) so the prompt,
//! the keystroke echo, and command output all serialize onto the one console
//! the tty owns. The shell needs no Console handle of its own (revoked in P5).
#![no_std]
#![no_main]

// Force-link oxbow-libc: the embedded Lua C archive references its symbols
// (malloc/setjmp/frexp/strncmp/…), but the shell's Rust never names a libc item,
// so without this the rlib is dropped and those symbols go undefined at link.
extern crate oxbow_libc as _;

use oxbow_abi::{
    Handle, MsgBuf, SysError, BOOT_CONSOLE, BOOT_FS_ROOT, BOOT_IMG_BADGE, BOOT_IMG_BETA, BOOT_NET_EP,
    BOOT_IMG_HELLO, BOOT_IMG_PONG,
    BOOT_IMG_CCHELLO, BOOT_IMG_DRIFT, BOOT_IMG_TCC, BOOT_IMG_LUA, BOOT_IMG_UPY, BOOT_IMG_QJS, BOOT_IMG_CURL, BOOT_IMG_CARES, BOOT_IMG_FFI, BOOT_IMG_WL, BOOT_IMG_XKB, BOOT_IMG_VTERM, BOOT_IMG_FT, BOOT_IMG_JAIL, BOOT_IMG_FSTEST, BOOT_MEM, BOOT_SESSION_CHAN, BOOT_TICK, BOOT_TTY,
    HANDLE_NULL, IdentRec, R_GRANT, R_IN, R_OUT, R_RECV,
    R_SEND, R_WAIT, R_WRITE, TAG_FS_CREATE, TAG_FS_MKDIR, TAG_FS_OPEN, TAG_FS_WRITE, TAG_TTY_FLUSH, TAG_TTY_READ, TAG_TTY_WRITE,
};
use oxbow_rt as rt;
use blake2::{Blake2b, Digest};
use blake2::digest::consts::U32;
/// Blake2b with a 32-byte digest — our password KDF primitive.
type Blake2b256 = Blake2b<U32>;

use core::ffi::c_void;

// ===========================================================================
// Embedded Lua 5.4 (csrc/luaglue.c). The shell is a hybrid: bash-style commands
// and pipelines run as today, while Lua syntax (control flow, expressions,
// assignments) runs in an in-process interpreter. The C glue owns the Lua state;
// Rust drives it one line at a time and lends it the tty via `ox_tty_write`.
// ===========================================================================
extern "C" {
    /// Create the persistent Lua state (opens the oxbow-safe libraries). NULL on OOM.
    fn ox_lua_new() -> *mut c_void;
    /// Evaluate one NUL-terminated line REPL-style (expression prints its value;
    /// statements just run). Errors are reported to the tty inside the glue.
    fn ox_lua_eval(state: *mut c_void, code: *const u8) -> i32;
    /// Write the string form of Lua global `name` into `out` (cap `cap`); returns
    /// the length, or -1 if the global is nil. Backs `$VAR` expansion.
    fn ox_lua_global(state: *mut c_void, name: *const u8, out: *mut u8, cap: i32) -> i32;
}

/// The shell's persistent Lua interpreter, created on first use. Globals set on
/// one line are visible on the next; chunk-local `local`s are not (Lua semantics).
static mut LUA_STATE: *mut c_void = core::ptr::null_mut();

/// The Lua state, lazily created on first use. NULL only if creation OOM'd.
fn lua_state() -> *mut c_void {
    unsafe {
        if LUA_STATE.is_null() {
            LUA_STATE = ox_lua_new();
        }
        LUA_STATE
    }
}

/// Look up Lua global `name` (a NUL-free identifier) as a string into `out`.
/// Returns the byte length, or None if unset/nil. Backs `$VAR` expansion.
fn lua_global(name: &[u8], out: &mut [u8]) -> Option<usize> {
    let st = lua_state();
    if st.is_null() {
        return None;
    }
    let mut nbuf = [0u8; 64];
    let n = core::cmp::min(name.len(), nbuf.len() - 1);
    nbuf[..n].copy_from_slice(&name[..n]);
    nbuf[n] = 0;
    let r = unsafe { ox_lua_global(st, nbuf.as_ptr(), out.as_mut_ptr(), out.len() as i32) };
    if r < 0 {
        None
    } else {
        Some(r as usize)
    }
}

/// The live shell context (Spawner + cwd + path), published before each Lua eval
/// so the Lua→shell bridge (`sh`/`sh_out`) can run commands. Valid only during a
/// synchronous `lua_eval` call; the shell is single-threaded so raw pointers into
/// `oxbow_main`'s locals are safe for that window.
struct ShellCtx {
    sp: *const Spawner,
    cwd: *mut Handle,
    path: *mut Path,
}
static mut SHELL_CTX: ShellCtx = ShellCtx {
    sp: core::ptr::null(),
    cwd: core::ptr::null_mut(),
    path: core::ptr::null_mut(),
};

/// Lua `sh("cmd")` (§83): run a command line through the full command layer
/// (pipes/operators/expansion, output to the tty) and return its exit status.
#[no_mangle]
pub extern "C" fn ox_shell_run(s: *const u8, n: usize) -> i32 {
    if s.is_null() {
        return 127;
    }
    let cmd = unsafe { core::slice::from_raw_parts(s, n) };
    unsafe {
        let ctx = &*core::ptr::addr_of!(SHELL_CTX);
        if ctx.sp.is_null() {
            return 127;
        }
        shell_run(cmd, &*ctx.sp, &mut *ctx.cwd, &mut *ctx.path);
    }
    last_status()
}

/// Lua `sh_out("cmd")` (§83): run a command and return its captured stdout length,
/// writing the bytes into `out` (capacity `cap`). -1 on no context.
#[no_mangle]
pub extern "C" fn ox_shell_capture(s: *const u8, n: usize, out: *mut u8, cap: i32) -> i32 {
    if s.is_null() || out.is_null() || cap <= 0 {
        return -1;
    }
    let cmd = unsafe { core::slice::from_raw_parts(s, n) };
    unsafe {
        let ctx = &*core::ptr::addr_of!(SHELL_CTX);
        if ctx.sp.is_null() {
            return -1;
        }
        let outbuf = core::slice::from_raw_parts_mut(out, cap as usize);
        capture(cmd, *ctx.cwd, &*ctx.sp, outbuf) as i32
    }
}

/// Called from C (luaglue.c) to emit Lua's output to the console the shell owns.
/// Routes through `tw` (TAG_TTY_WRITE) — the shell has no libc stdout FILE.
#[no_mangle]
pub extern "C" fn ox_tty_write(p: *const u8, n: usize) {
    if p.is_null() || n == 0 {
        return;
    }
    tw(unsafe { core::slice::from_raw_parts(p, n) });
}

/// Evaluate one line of Lua, creating the interpreter on first use. The C API
/// needs a NUL terminator, so copy into a stack buffer (lines are <=256 bytes).
fn lua_eval(line: &[u8]) {
    let st = lua_state();
    if st.is_null() {
        tw(b"lua: cannot create interpreter (out of memory)\n");
        return;
    }
    let mut buf = [0u8; 512];
    let n = core::cmp::min(line.len(), buf.len() - 1);
    buf[..n].copy_from_slice(&line[..n]);
    buf[n] = 0;
    unsafe { ox_lua_eval(st, buf.as_ptr()) };
}

/// Word-expansion for a command line (§82), bash-style, in one left-to-right pass:
///   '...'    literal — no expansion, quotes removed
///   "..."    grouped — `$VAR` still expands, quotes removed
///   $name    / ${name}  → the value of Lua global `name` (a shell var IS a Lua
///                          global); unset → empty
/// Other bytes copy through. `$(...)` command substitution and `*` globbing are
/// layered on by the caller. Returns the expanded length written to `out`.
fn expand(input: &[u8], cwd: Handle, sp: &Spawner, out: &mut [u8]) -> usize {
    let mut o = 0;
    let mut i = 0;
    while i < input.len() {
        let c = input[i];
        match c {
            b'\'' => {
                // Single quotes: copy verbatim until the closing quote.
                i += 1;
                while i < input.len() && input[i] != b'\'' {
                    pb(out, &mut o, input[i]);
                    i += 1;
                }
                i += 1; // skip closing quote (or run off end)
            }
            b'"' => {
                // Double quotes: copy, but expand `$` inside.
                i += 1;
                while i < input.len() && input[i] != b'"' {
                    if input[i] == b'$' {
                        i += expand_dollar(&input[i..], cwd, sp, out, &mut o);
                    } else {
                        pb(out, &mut o, input[i]);
                        i += 1;
                    }
                }
                i += 1; // skip closing quote
            }
            b'$' => {
                i += expand_dollar(&input[i..], cwd, sp, out, &mut o);
            }
            _ => {
                pb(out, &mut o, c);
                i += 1;
            }
        }
    }
    o
}

/// Append one byte to `out` at `*o`, bounded by capacity.
fn pb(out: &mut [u8], o: &mut usize, b: u8) {
    if *o < out.len() {
        out[*o] = b;
        *o += 1;
    }
}

/// Glob match: does `pat` (with `*` = any run, `?` = any one char) match `name`?
/// Iterative with `*` backtracking — no recursion, no allocation.
fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while n < name.len() {
        if p < pat.len() && (pat[p] == name[n] || pat[p] == b'?') {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = n;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Filename globbing (§82): expand each space-separated word that contains `*`/`?`
/// into the matching entries of `cwd` (sorted by directory order, hidden dotfiles
/// excluded unless the pattern starts with `.`). A pattern that matches nothing is
/// left literal (bash default). Words without wildcards pass through unchanged.
fn glob_line(input: &[u8], cwd: Handle, out: &mut [u8]) -> usize {
    let mut o = 0;
    let mut first = true;
    for word in input.split(|&b| b == b' ') {
        if word.is_empty() {
            continue;
        }
        if !first {
            pb(out, &mut o, b' ');
        }
        first = false;
        if word.contains(&b'*') || word.contains(&b'?') {
            let mut matched = 0;
            let mut idx = 0u64;
            while let Some((name, _kind)) = rt::fs::readdir(cwd, idx) {
                idx += 1;
                if name.first() == Some(&b'.') && word.first() != Some(&b'.') {
                    continue; // skip dotfiles unless the pattern asks for them
                }
                if glob_match(word, &name) {
                    if matched > 0 {
                        pb(out, &mut o, b' ');
                    }
                    for &b in &name {
                        pb(out, &mut o, b);
                    }
                    matched += 1;
                }
            }
            if matched == 0 {
                for &b in word {
                    pb(out, &mut o, b); // no match: keep the literal pattern
                }
            }
        } else {
            for &b in word {
                pb(out, &mut o, b);
            }
        }
    }
    o
}

/// Full word-expansion for a command: `$VAR`/quoting (`expand`) then `*` globbing
/// (`glob_line`), in that order. The single entry point the command layer calls.
fn expand_line(input: &[u8], cwd: Handle, sp: &Spawner, out: &mut [u8]) -> usize {
    let mut tmp = [0u8; 512];
    let n = expand(input, cwd, sp, &mut tmp);
    glob_line(&tmp[..n], cwd, out)
}

/// Expand a `$` reference at the start of `s` (`$name` or `${name}`), appending
/// the Lua-global value to `out`. Returns the number of input bytes consumed. A
/// lone `$` (no name) is copied literally.
fn expand_dollar(s: &[u8], cwd: Handle, sp: &Spawner, out: &mut [u8], o: &mut usize) -> usize {
    // $(command) — run it, substitute its stdout (trailing newlines stripped,
    // internal newlines -> spaces, like an unquoted command substitution).
    if s.len() >= 2 && s[1] == b'(' {
        let mut depth = 1;
        let mut j = 2;
        while j < s.len() && depth > 0 {
            match s[j] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let inner = &s[2..j];
        let consumed = if j < s.len() { j + 1 } else { j };
        let mut cap = [0u8; 512];
        let n = capture(inner, cwd, sp, &mut cap);
        let mut end = n;
        while end > 0 && cap[end - 1] == b'\n' {
            end -= 1; // strip trailing newlines
        }
        for &b in &cap[..end] {
            pb(out, o, if b == b'\n' { b' ' } else { b });
        }
        return consumed;
    }
    let (name, consumed): (&[u8], usize) = if s.len() >= 2 && s[1] == b'{' {
        // ${name}
        let mut j = 2;
        while j < s.len() && s[j] != b'}' {
            j += 1;
        }
        (&s[2..j], if j < s.len() { j + 1 } else { j })
    } else {
        // $name — name is [A-Za-z_][A-Za-z0-9_]*
        let mut j = 1;
        while j < s.len() && (s[j].is_ascii_alphanumeric() || s[j] == b'_') {
            j += 1;
        }
        (&s[1..j], j)
    };
    if name.is_empty() {
        pb(out, o, b'$'); // a lone `$` is literal
        return 1;
    }
    let mut val = [0u8; 256];
    if let Some(n) = lua_global(name, &mut val) {
        for &b in &val[..n] {
            pb(out, o, b);
        }
    }
    consumed
}

/// Run `cmd` and capture its stdout into `out` (§82) — the engine behind `$(…)`.
/// Spawns the command with stdout wired to a pipe, waits for it to exit, then
/// drains the pipe. `echo` is computed directly (a builtin). Captured output is
/// bounded by the pipe buffer (~8 KiB) and by `out`. Returns the byte count.
fn capture(cmd: &[u8], cwd: Handle, sp: &Spawner, out: &mut [u8]) -> usize {
    let (verb, rest) = split_cmd(trim(cmd));
    // echo is a builtin: its output is just its (expanded) argument.
    if verb == b"echo" {
        return expand_line(rest, cwd, sp, out);
    }
    // Spawnable command: pipe its stdout back to us.
    let pipe = match rt::sys_pipe() {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let wend = rt::sys_attenuate(pipe, R_OUT | R_GRANT).unwrap_or(HANDLE_NULL);
    let rend = rt::sys_attenuate(pipe, R_IN).unwrap_or(HANDLE_NULL);
    let _ = rt::sys_close(pipe);
    if wend == HANDLE_NULL || rend == HANDLE_NULL {
        let _ = rt::sys_close(wend);
        let _ = rt::sys_close(rend);
        return 0;
    }
    // Use sp.exit (the general exit notifier, idle during expansion) — NOT a
    // pexits[] slot, which a surrounding pipeline stage may be about to use.
    // $() substitution has no Path handle; resolve from the session root (bare
    // /bin names are the common case here, which don't consult it).
    if !spawn_stage(cmd, cwd, &Path::root(), sp, HANDLE_NULL, wend, sp.exit) {
        let _ = rt::sys_close(wend);
        let _ = rt::sys_close(rend);
        return 0;
    }
    // The command writes its output into the pipe buffer (assumed < 8 KiB) and
    // exits; then we mark EOF and drain. (Larger output would block the writer
    // with no reader — a documented limit on captured size.)
    let _ = rt::sys_notif_wait(sp.exit);
    let _ = rt::sys_pipe_eof(wend);
    let mut o = 0;
    loop {
        let mut buf = [0u8; 64];
        let n = rt::sys_pipe_read(rend, &mut buf);
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if o < out.len() {
                out[o] = b;
                o += 1;
            }
        }
    }
    let _ = rt::sys_close(wend);
    let _ = rt::sys_close(rend);
    o
}

/// Decide whether a line is Lua (control flow, `print`, or an assignment) rather
/// than a bash-style command. Conservative: commands are `verb args`, Lua
/// assignments are `lvalue = rvalue` — the `=` right after an identifier is the
/// discriminator, and a leading keyword settles the rest.
fn looks_like_lua(line: &[u8]) -> bool {
    let t = trim(line);
    if t.is_empty() {
        return false;
    }
    let (w, _) = split_cmd(t);
    if matches!(
        w,
        b"local"
            | b"if"
            | b"for"
            | b"while"
            | b"function"
            | b"repeat"
            | b"return"
            | b"do"
            | b"end"
            | b"else"
            | b"elseif"
            | b"until"
            | b"goto"
            | b"break"
    ) {
        return true;
    }
    if is_lua_call(t) {
        return true;
    }
    is_assignment(t)
}

/// True for a Lua function-call line: `name(...)` (any function — commands never
/// use `(`), or a known Lua function called with a bare string literal —
/// `sh "ls"`, `print "hi"`. The known-function gate keeps `echo "hi"` a command.
fn is_lua_call(t: &[u8]) -> bool {
    if t.is_empty() || !(t[0].is_ascii_alphabetic() || t[0] == b'_') {
        return false;
    }
    let mut i = 0;
    while i < t.len() && (t[i].is_ascii_alphanumeric() || t[i] == b'_') {
        i += 1;
    }
    let ident = &t[..i];
    if i < t.len() && t[i] == b'(' {
        return true; // name( … ) — a call
    }
    // known Lua fn + whitespace + a string literal (Lua's `f "str"` sugar)
    let mut j = i;
    while j < t.len() && t[j] == b' ' {
        j += 1;
    }
    if j > i && j < t.len() && (t[j] == b'"' || t[j] == b'\'') {
        return matches!(ident, b"sh" | b"sh_out" | b"print");
    }
    false
}

/// True for `name = ...` / `t.k = ...` / `a[i] = ...` (a Lua assignment), but not
/// for comparisons (`==`, `~=`, `<=`, `>=`) or commands (`verb arg`).
fn is_assignment(t: &[u8]) -> bool {
    if t.is_empty() || !(t[0].is_ascii_alphabetic() || t[0] == b'_') {
        return false;
    }
    let mut i = 0;
    while i < t.len() {
        let c = t[i];
        if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'[' | b']' | b'"' | b'\'') {
            i += 1;
        } else {
            break;
        }
    }
    while i < t.len() && t[i] == b' ' {
        i += 1;
    }
    // Must be a single '=' (assignment), not '==' (comparison).
    i < t.len() && t[i] == b'=' && (i + 1 >= t.len() || t[i + 1] != b'=')
}

/// The current-directory path string, tracked alongside the cwd capability so
/// the prompt can show it (a Unix shell shows where you are).
#[derive(Clone, Copy)]
struct Path {
    buf: [u8; 128],
    len: usize,
}
impl Path {
    fn root() -> Self {
        let mut buf = [0u8; 128];
        buf[0] = b'/';
        Path { buf, len: 1 }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    fn push(&mut self, comp: &[u8]) {
        if self.buf[self.len - 1] != b'/' && self.len < self.buf.len() {
            self.buf[self.len] = b'/';
            self.len += 1;
        }
        for &c in comp {
            if self.len < self.buf.len() {
                self.buf[self.len] = c;
                self.len += 1;
            }
        }
    }
    fn pop(&mut self) {
        if self.len <= 1 {
            self.len = 1;
            return;
        }
        let mut i = self.len;
        while i > 1 && self.buf[i - 1] != b'/' {
            i -= 1;
        }
        self.len = if i > 1 { i - 1 } else { 1 };
    }
    /// Update the path for a `cd` target (handles `/`, `..`, `.`, multi-component).
    fn apply(&mut self, name: &[u8]) {
        if name.is_empty() || name == b"/" {
            self.len = 1;
            self.buf[0] = b'/';
            return;
        }
        if name[0] == b'/' {
            self.len = 1;
            self.buf[0] = b'/';
        }
        for comp in name.split(|&b| b == b'/') {
            match comp {
                b"" | b"." => {}
                b".." => self.pop(),
                _ => self.push(comp),
            }
        }
    }
}

/// Capabilities the shell mints once at startup to launch programs with.
struct Spawner {
    /// An attenuated tty send endpoint, handed to children as their stdout.
    stdout: Handle,
    /// A notification the kernel signals when a spawned child exits.
    exit: Handle,
    /// A spare endpoint used to wire up child↔child IPC (e.g. pong↔beta).
    ep: Handle,
    /// Reusable per-stage exit notifications for pipelines (§81). Created once so
    /// the shell never churns the kernel notif pool — that pool isn't freed on
    /// close, so a create-per-pipeline would leak it dry after a few commands.
    /// One per possible pipeline stage; pipelines wait on them and drain them.
    pexits: [Handle; 4],
}

/// Write a byte string to the console via the tty. Chunks into <=63-byte,
/// NUL-terminated TAG_TTY_WRITE messages so payloads longer than one MsgBuf
/// (e.g. the help text) still go out whole.
fn tw(s: &[u8]) {
    let mut off = 0;
    while off < s.len() {
        let n = core::cmp::min(63, s.len() - off);
        let mut m = MsgBuf::new(TAG_TTY_WRITE);
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(s[off..].as_ptr(), dst, n);
            *dst.add(n) = 0;
        }
        m.data_len = ((n + 1 + 7) / 8) as u32;
        let _ = rt::sys_send(BOOT_TTY, &m);
        off += n;
    }
}

/// Read one line from the tty (blocks until Enter). The tty streams the line in
/// chunks (data[0]=count, data[1]=more, payload at offset 16); accumulate them
/// into `buf` until `more` is 0. Returns the line length.
fn read_line(buf: &mut [u8; 256]) -> usize {
    let mut total = 0usize;
    loop {
        let mut m = MsgBuf::new(TAG_TTY_READ);
        if rt::sys_call(BOOT_TTY, &mut m).is_err() {
            return total;
        }
        let n = core::cmp::min(m.data[0] as usize, 48);
        let more = m.data[1];
        let take = core::cmp::min(n, buf.len() - total);
        unsafe {
            core::ptr::copy_nonoverlapping(
                (m.data.as_ptr() as *const u8).add(16),
                buf.as_mut_ptr().add(total),
                take,
            );
        }
        total += take;
        if more == 0 {
            break;
        }
    }
    total
}

/// Split a line into (command, rest-of-line) at the first run of spaces.
/// `rest` keeps everything after the first space, leading spaces trimmed.
fn split_cmd(line: &[u8]) -> (&[u8], &[u8]) {
    let cmd_end = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
    let (cmd, after) = line.split_at(cmd_end);
    // Trim leading spaces from the remainder.
    let mut i = 0;
    while i < after.len() && after[i] == b' ' {
        i += 1;
    }
    (cmd, &after[i..])
}

/// Block until `n` spawned children have exited (the kernel signals `exit`,
/// a counting notification, once per death).
fn wait_exits(sp: &Spawner, n: u64) {
    let mut exited = 0u64;
    while exited < n {
        match rt::sys_notif_wait(sp.exit) {
            Ok(c) => exited += c,
            Err(_) => break,
        }
    }
}

/// Spawn a program, granting it `cap0` at slot 1 (BOOT_EP) and stdout at slot 2,
/// then wait for it to exit. `cap0 = HANDLE_NULL` for a program that needs no
/// input capability (e.g. hello). For ls/cat, `cap0` is the dir/file capability
/// the shell hands over — the spawned coreutil never sees a name, just the cap.
fn spawn_with(image: Handle, cap0: Handle, arg: &[u8], sp: &Spawner) {
    spawn_with_budget(image, cap0, arg, 0, sp);
}

/// Like `spawn_with`, but requests a specific child Memory budget (0 = default).
/// tcc needs a large working set to compile, so it asks for a big budget.
fn spawn_with_budget(image: Handle, cap0: Handle, arg: &[u8], budget: u64, sp: &Spawner) {
    let mut m = MsgBuf::new(0);
    // data[0] = budget (0 = default). Real argv (§13): data[1] = pointer to the
    // argument string, data[2] = its length — the kernel copies it into the
    // child's argv page (up to a full page, lifting the old 55-byte limit). `arg`
    // stays valid for this synchronous spawn call.
    m.data[0] = budget;
    m.data[1] = arg.as_ptr() as u64;
    m.data[2] = arg.len() as u64;
    m.data_len = 3;
    // §24: children inherit OUR identity (whoami stays consistent across exec).
    rt::msg_set_identity(&mut m, cur_ident());
    m.handle_count = 4;
    m.handles[0] = cap0; // slot 1 = BOOT_EP (a file/dir cap, or NULL)
    m.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    m.handles[2] = HANDLE_NULL; // slot 4 = BOOT_TICK (unused here)
    m.handles[3] = BOOT_NET_EP; // slot 20 = BOOT_NET_EP (network access)
    match rt::sys_spawn(image, BOOT_MEM, &m, sp.exit) {
        Ok(_) => {
            wait_exits(sp, 1);
            set_status(rt::sys_notif_status(sp.exit)); // child's exit code → $?
        }
        Err(e) => {
            tw(b"run: spawn failed (err ");
            tw_dec(e as u8);
            tw(b")\n");
            set_status(127);
        }
    }
}

/// `cc <src> -o <out>`: compile + statically link a C program to a STANDALONE
/// oxbow binary via tcc. Expands to `tcc -static <args> /lib/c.a` — `/lib/c.a` is
/// liboxbow_libc.a, the C library archive that makes the output self-contained
/// (no dynamic linker; tcc fills the GOT at link time). `-static` is essential:
/// tcc defaults to a dynamic executable whose GOT a runtime ld.so would fill, but
/// oxbow has none. Run the result with `exec <out>`. The whole toolchain runs on
/// oxbow — the self-hosting milestone (ABI §35).
fn cc_cmd(cwd: Handle, rest: &[u8], sp: &Spawner) {
    if rest.is_empty() {
        tw(b"cc: usage: cc <src.c> -o <out>   (then: exec <out>)\n");
        return;
    }
    let prefix: &[u8] = b"-static ";
    let suffix: &[u8] = b" /lib/c.a";
    let mut arg = [0u8; 1024];
    if prefix.len() + rest.len() + suffix.len() > arg.len() {
        tw(b"cc: command too long\n");
        return;
    }
    let mut p = 0;
    for src in [prefix, rest, suffix] {
        arg[p..p + src.len()].copy_from_slice(src);
        p += src.len();
    }
    spawn_with_budget(BOOT_IMG_TCC, cwd, &arg[..p], 48 * 1024 * 1024, sp);
}


/// Scratch buffer holding an ELF read off the filesystem for `exec` (§33). Sized
/// for a stripped no_std binary with headroom; a larger image truncates safely
/// (read_all stops at the buffer end and try_validate then rejects it).
const ELF_BUF_CAP: usize = 2 * 1024 * 1024;
static mut ELF_BUF: [u8; ELF_BUF_CAP] = [0; ELF_BUF_CAP];

/// Slurp an entire file capability into `buf` via the 56-byte FS_READ protocol,
/// looping on the read offset until EOF. Returns the byte count read.
unsafe fn read_all(cap: Handle, buf: &mut [u8]) -> usize {
    let mut off = 0usize;
    loop {
        let mut m = MsgBuf::new(oxbow_abi::TAG_FS_READ);
        m.data[0] = off as u64;
        m.data_len = 1;
        if rt::sys_call(cap, &mut m).is_err() {
            break;
        }
        let count = core::cmp::min(m.data[0] as usize, 56);
        if count == 0 || off + count > buf.len() {
            break;
        }
        core::ptr::copy_nonoverlapping(
            (m.data.as_ptr() as *const u8).add(8),
            buf.as_mut_ptr().add(off),
            count,
        );
        off += count;
    }
    off
}

/// §94: the `/bin` directory capability — the system tool set, reachable by EVERY
/// logged-in user. The shell opens it once from the broad root authority
/// (`BOOT_FS_ROOT`, the login machinery it still holds) and uses it to resolve
/// bare command names. It is independent of `session_root()`: a confined user's
/// namespace can't name `/bin` (leading `/` collapses onto their home), so the
/// shell holds this cap on everyone's behalf. Programs OUTSIDE /bin + outside the
/// user's home stay unreachable — that's the "not every program for everyone"
/// half, enforced purely by which capabilities exist, no permission bits.
static mut BIN_DIR: Handle = HANDLE_NULL;

fn bin_dir() -> Handle {
    unsafe { BIN_DIR }
}

/// Open `/bin` from the root authority once at startup and cache the dir cap.
fn open_bin_dir() {
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, b"/bin");
    if rt::sys_call(BOOT_FS_ROOT, &mut m).is_ok()
        && m.data[0] == 0
        && m.data[1] == oxbow_abi::FS_DIR
    {
        unsafe {
            BIN_DIR = m.handles[0];
        }
    }
}

/// Open `name` relative to directory cap `dir`; if it is a regular FILE, slurp it
/// into ELF_BUF and return its byte length. Returns `None` if `name` is absent,
/// is a directory, or is empty — so a PATH search can fall through to the next dir.
fn slurp_program(dir: Handle, name: &[u8]) -> Option<usize> {
    if dir == HANDLE_NULL {
        return None;
    }
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, name);
    if rt::sys_call(dir, &mut m).is_err() || m.data[0] != 0 {
        return None;
    }
    let file_cap = m.handles[0];
    if m.data[1] != oxbow_abi::FS_FILE {
        let _ = rt::sys_close(file_cap);
        return None;
    }
    let len = unsafe {
        let buf = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(ELF_BUF) as *mut u8,
            ELF_BUF_CAP,
        );
        read_all(file_cap, buf)
    };
    let _ = rt::sys_close(file_cap);
    if len == 0 {
        None
    } else {
        Some(len)
    }
}

/// §94: resolve a command `verb` to a program FILE and slurp it into ELF_BUF.
/// A name containing `/` is a path resolved in the user's session namespace; a
/// bare name searches `/bin` (system tools, every user) then `bin/` in the user's
/// own home (per-user programs). Returns the ELF length, or `None` if unresolved.
fn find_program(verb: &[u8], path: &Path) -> Option<usize> {
    if verb.contains(&b'/') {
        let mut t = *path;
        t.apply(verb);
        return slurp_program(session_root(), t.as_bytes());
    }
    if let Some(l) = slurp_program(bin_dir(), verb) {
        return Some(l);
    }
    let mut name = [0u8; 4 + 64];
    name[..4].copy_from_slice(b"bin/");
    let n = core::cmp::min(verb.len(), 64);
    name[4..4 + n].copy_from_slice(&verb[..n]);
    slurp_program(session_root(), &name[..4 + n])
}

/// Run the program currently in `ELF_BUF[..len]` to completion (exec-from-fs, ABI
/// §33): cwd dir cap at slot 1, stdout at slot 2, the net cap at slot 20, argv =
/// `args`, and our identity. The exit status flows into `$?`.
fn run_program(len: usize, cwd: Handle, args: &[u8], sp: &Spawner) {
    let mut sm = MsgBuf::new(0);
    sm.data[0] = 0;
    sm.data[1] = args.as_ptr() as u64;
    sm.data[2] = args.len() as u64;
    sm.data_len = 3;
    rt::msg_set_identity(&mut sm, cur_ident()); // §44: program inherits our identity
    sm.handle_count = 4;
    sm.handles[0] = cwd;
    sm.handles[1] = sp.stdout;
    sm.handles[2] = HANDLE_NULL; // slot 4 (stdin) unused outside a pipeline
    sm.handles[3] = BOOT_NET_EP; // slot 20 = network access
    let elf = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(ELF_BUF) as *const u8, len) };
    match rt::sys_spawn_bytes(elf, BOOT_MEM, &sm, sp.exit) {
        Ok(_) => {
            wait_exits(sp, 1);
            set_status(rt::sys_notif_status(sp.exit));
        }
        Err(_) => {
            tw(b"exec: not a valid program (spawn rejected)\n");
            set_status(126);
        }
    }
}

/// §94: resolve a non-builtin command to a program file and run it. Returns
/// `false` only if nothing matched (so the caller prints "command not found").
fn path_exec(cmd: &[u8], rest: &[u8], cwd: Handle, path: &Path, sp: &Spawner) -> bool {
    match find_program(cmd, path) {
        Some(len) => {
            run_program(len, cwd, rest, sp);
            true
        }
        None => false,
    }
}

/// `exec <path> [args]`: explicitly run an ELF from the filesystem (exec-from-fs,
/// ABI §33). Equivalent to just typing the path — kept for clarity and scripts.
fn exec_cmd(cwd: Handle, path: &Path, arg_line: &[u8], sp: &Spawner) {
    let (pathname, rest) = split_cmd(arg_line);
    if pathname.is_empty() {
        tw(b"exec: usage: exec <path> [args]\n");
        return;
    }
    if !path_exec(pathname, rest, cwd, path, sp) {
        tw(b"exec: ");
        tw(pathname);
        tw(b": not found\n");
        set_status(127);
    }
}

/// `whoami`: our login name (§44). The shell's current identity.
fn whoami_cmd() {
    tw(cur_name());
    tw(b"\n");
}

/// Print `gid` then `(name)` if the gid has a known name.
fn tw_gid(gid: u32) {
    tw_dec_u32(gid);
    let n = gid_name(gid);
    if !n.is_empty() {
        tw(b"(");
        tw(n);
        tw(b")");
    }
}

/// `id`: uid/gid + supplementary groups, the POSIX way.
fn id_cmd() {
    let id = cur_ident();
    tw(b"uid=");
    tw_dec_u32(id.uid);
    tw(b"(");
    tw(cur_name());
    tw(b") gid=");
    tw_gid(id.gid);
    let n = id.ngroups as usize;
    if n > 0 {
        tw(b" groups=");
        for i in 0..n {
            if i > 0 {
                tw(b",");
            }
            tw_gid(id.groups[i]);
        }
    }
    tw(b"\n");
}

/// `groups`: our group ids, space-separated.
fn groups_cmd() {
    let id = cur_ident();
    let n = id.ngroups as usize;
    if n == 0 {
        tw_dec_u32(id.gid);
    } else {
        for i in 0..n {
            if i > 0 {
                tw(b" ");
            }
            tw_dec_u32(id.groups[i]);
        }
    }
    tw(b"\n");
}

/// Write a u32 as decimal ASCII to the tty.
fn tw_dec_u32(mut n: u32) {
    let mut b = [0u8; 10];
    let mut i = 10;
    loop {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    tw(&b[i..]);
}

/// `jail`: the capability-confinement showcase. Hand a hostile program a
/// capability to exactly ONE directory (/tmp) and nothing else, then let it try
/// every escape and watch the kernel deny each. The dir cap is the L3 test
/// subject — jail can act inside /tmp but must not be able to walk above it.
fn jail_cmd(sp: &Spawner) {
    // Open /tmp under the root to mint a confined dir capability for jail.
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    let name = b"tmp";
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), dst, name.len());
        *dst.add(name.len()) = 0;
    }
    m.data_len = 8;
    let dir = if rt::sys_call(BOOT_FS_ROOT, &mut m).is_ok() && m.data[0] == 0 {
        m.handles[0]
    } else {
        HANDLE_NULL // /tmp missing — jail still runs; its L3 test just no-ops
    };
    spawn_with(BOOT_IMG_JAIL, dir, b"", sp);
    if dir != HANDLE_NULL {
        let _ = rt::sys_close(dir);
    }
}

/// `rand`: print a few words from the kernel CSPRNG (getentropy). Real entropy
/// now — RDSEED/RDRAND-seeded ChaCha20 — so the values differ every boot.
fn rand_cmd() {
    let mut buf = [0u8; 32];
    if rt::sys_getentropy(&mut buf).is_err() {
        tw(b"rand: getentropy failed\n");
        return;
    }
    let hexd = b"0123456789abcdef";
    tw(b"rand:");
    for chunk in buf.chunks(4) {
        tw(b" ");
        for &byte in chunk {
            tw(&[hexd[(byte >> 4) as usize], hexd[(byte & 0xf) as usize]]);
        }
    }
    tw(b"\n");
}

/// `chantest`: exercise the byte+capability channel (§40) — the Wayland/socketpair
/// transport. Create a pair, write a magic word into a fresh frame, send "PING"
/// plus that frame CAP over one end, receive on the other, map the received cap,
/// and confirm the magic — proving both byte streaming and capability passing.
fn chantest() {
    let Some((h0, h1)) = rt::channel::pair() else {
        tw(b"chan: pair failed\n");
        return;
    };
    let Ok(frame) = rt::sys_frame_alloc(BOOT_MEM) else {
        tw(b"chan: frame alloc failed\n");
        return;
    };
    const V1: u64 = 0x3C00_0000;
    const V2: u64 = 0x3C10_0000;
    const PROT_RW: u64 = oxbow_abi::PROT_READ | oxbow_abi::PROT_WRITE;
    if rt::sys_frame_map(frame, V1, PROT_RW).is_err() {
        tw(b"chan: map1 failed\n");
        return;
    }
    unsafe { core::ptr::write_volatile(V1 as *mut u32, 0xCAFE_BABE) };

    let _ = rt::channel::send(h0, b"PING", &[frame]);

    let mut buf = [0u8; 16];
    let mut caps = [0u32; 4];
    let Some((n, nc)) = rt::channel::recv(h1, &mut buf, &mut caps, false) else {
        tw(b"chan: recv failed\n");
        return;
    };
    tw(b"chan: got ");
    tw_dec(n as u8);
    tw(b" bytes \"");
    tw(&buf[..n]);
    tw(b"\" + ");
    tw_dec(nc as u8);
    tw(b" cap(s)\n");

    if nc >= 1 && rt::sys_frame_map(caps[0], V2, PROT_RW).is_ok() {
        let magic = unsafe { core::ptr::read_volatile(V2 as *const u32) };
        if magic == 0xCAFE_BABE {
            tw(b"chan: capability passed OK (frame magic matches across the channel)\n");
        } else {
            tw(b"chan: FAIL - magic mismatch\n");
        }
    } else if nc >= 1 {
        tw(b"chan: FAIL - could not map received cap\n");
    } else {
        tw(b"chan: FAIL - no cap received\n");
    }
    rt::channel::close(h0);
    rt::channel::close(h1);
}

/// `sync`: ask the fs to persist its writable tree to disk (TAG_FS_SYNC on the
/// root dir cap). The tree is restored automatically at the next boot.
fn sync_cmd() {
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_SYNC);
    if rt::sys_call(BOOT_FS_ROOT, &mut m).is_err() || m.data[0] != 0 {
        tw(b"sync: failed (no disk?)\n");
        return;
    }
    tw(b"sync: persisted ");
    tw_dec_u32(m.data[1] as u32);
    tw(b" entries to disk\n");
}

/// Write a byte as decimal ASCII to the tty (for printing IP octets).
fn tw_dec(n: u8) {
    let mut b = [0u8; 3];
    let mut i = 3;
    let mut v = n;
    loop {
        i -= 1;
        b[i] = b'0' + v % 10;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    tw(&b[i..]);
}

/// `dns <hostname>`: resolve a name over the net server's UDP socket capability
/// API. We hold only BOOT_NET_EP (the NET_CTL control cap); `udp::bind` mints us
/// a socket cap, and the whole DNS exchange rides it — net never sees a name,
/// just a UDP datagram on a bound port. This crosses the shell↔net process
/// boundary entirely through capabilities.
fn dns_cmd(name: &[u8]) {
    if name.is_empty() {
        tw(b"dns: usage: dns <hostname>\n");
        return;
    }
    let Ok(name_str) = core::str::from_utf8(name) else {
        tw(b"dns: bad name\n");
        return;
    };
    // Attach the shared UDP frame ONCE per process: there is no unmap syscall, so
    // a second sys_frame_map at UDP_XFER would fail. Cache the buffer pointer.
    static mut DNS_BUF: *mut u8 = core::ptr::null_mut();
    let buf = unsafe {
        if DNS_BUF.is_null() {
            DNS_BUF = rt::udp::attach(BOOT_NET_EP).unwrap_or(core::ptr::null_mut());
        }
        DNS_BUF
    };
    if buf.is_null() {
        tw(b"dns: attach failed (no net server?)\n");
        return;
    }
    let Some((sock, _port)) = rt::udp::bind(BOOT_NET_EP, 0) else {
        tw(b"dns: bind failed (no net server?)\n");
        return;
    };
    let q = rt::dns::query(0x1234, name_str);
    let server = rt::udp::dns_server(BOOT_NET_EP); // DHCP-leased resolver, not hardcoded
    // Query + response ride the shared frame (large path — no 40-byte inline cap).
    unsafe { core::ptr::copy_nonoverlapping(q.as_ptr(), buf, q.len()) };
    if !rt::udp::sendv(sock, server, 53, q.len()) {
        tw(b"dns: send failed\n");
        rt::udp::close(sock);
        return;
    }
    // recvv is non-blocking; poll with a deadline so a lost reply doesn't hang.
    let mut n = 0;
    let deadline = rt::sys_uptime_ms() + 3000;
    while rt::sys_uptime_ms() < deadline {
        n = rt::udp::recvv(sock);
        if n > 0 {
            break;
        }
    }
    rt::udp::close(sock);
    let resp = unsafe { core::slice::from_raw_parts(buf, n) };
    match rt::dns::first_a(resp) {
        Some(ip) => {
            tw(name);
            tw(b" -> ");
            tw_dec(ip[0]);
            tw(b".");
            tw_dec(ip[1]);
            tw(b".");
            tw_dec(ip[2]);
            tw(b".");
            tw_dec(ip[3]);
            tw(b"\n");
        }
        None => {
            tw(name);
            tw(b": no A record\n");
        }
    }
}

/// Parse a dotted-quad IPv4 address.
fn parse_ip(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    let mut val: u32 = 0;
    let mut have = false;
    for &c in s {
        if c == b'.' {
            if !have || idx >= 3 {
                return None;
            }
            octets[idx] = val as u8;
            idx += 1;
            val = 0;
            have = false;
        } else if c.is_ascii_digit() {
            val = val * 10 + (c - b'0') as u32;
            if val > 255 {
                return None;
            }
            have = true;
        } else {
            return None;
        }
    }
    if !have || idx != 3 {
        return None;
    }
    octets[3] = val as u8;
    Some(octets)
}

/// `http <ip>`: open a TCP connection to <ip>:80 over the net server's socket
/// capability API (smoltcp does the TCP), send a minimal HTTP/1.0 GET, and print
/// the response. We hold only BOOT_NET_EP; `tcp::connect` mints us a socket cap.
fn http_cmd(args: &[u8]) {
    let (host, rest) = split_cmd(args);
    let Some(ip) = parse_ip(host) else {
        tw(b"http: usage: http <a.b.c.d> [port]\n");
        return;
    };
    let (port_tok, _) = split_cmd(rest);
    let mut port: u16 = 80;
    if !port_tok.is_empty() {
        let mut v: u32 = 0;
        for &c in port_tok {
            if c.is_ascii_digit() {
                v = v * 10 + (c - b'0') as u32;
            }
        }
        if v > 0 && v <= 65535 {
            port = v as u16;
        }
    }
    let Some(sock) = rt::tcp::connect(BOOT_NET_EP, ip, port) else {
        tw(b"http: connect failed (refused/timeout)\n");
        return;
    };
    tw(b"http: connected, GET /\n");
    if !rt::tcp::send(sock, b"GET / HTTP/1.0\r\n\r\n") {
        tw(b"http: send failed\n");
        rt::tcp::close(sock);
        return;
    }
    let mut buf = [0u8; 64];
    let mut total = 0usize;
    for _ in 0..8 {
        let n = rt::tcp::recv(sock, &mut buf);
        if n == 0 {
            break;
        }
        tw(&buf[..n]);
        total += n;
    }
    if total == 0 {
        tw(b"http: no response\n");
    } else {
        tw(b"\n");
    }
    rt::tcp::close(sock);
}

/// `run pong`: launch the pong↔beta demo pair, wiring an endpoint between them
/// (beta gets the recv side, pong the send side) and delegating the tick to pong.
/// Proves multi-handle grant-at-spawn and child↔child IPC.
fn run_pong(sp: &Spawner) {
    let ep_recv = rt::sys_attenuate(sp.ep, R_RECV | R_GRANT);
    let ep_send = rt::sys_attenuate(sp.ep, R_SEND | R_GRANT);
    let tick_w = rt::sys_attenuate(BOOT_TICK, R_WAIT | R_GRANT);
    let (ep_recv, ep_send, tick_w) = match (ep_recv, ep_send, tick_w) {
        (Ok(r), Ok(s), Ok(t)) => (r, s, t),
        _ => {
            tw(b"run: could not set up pong channel\n");
            return;
        }
    };
    // beta (receiver) first, so it is ready to recv when pong sends.
    let mut mb = MsgBuf::new(0);
    mb.data_len = 1;
    mb.handle_count = 2;
    mb.handles[0] = ep_recv; // slot 1 = BOOT_EP (recv)
    mb.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    let beta_ok = rt::sys_spawn(BOOT_IMG_BETA, BOOT_MEM, &mb, sp.exit).is_ok();
    // pong (sender) gets the send endpoint, stdout, and the tick.
    let mut mp = MsgBuf::new(0);
    mp.data_len = 1;
    mp.handle_count = 3;
    mp.handles[0] = ep_send; // slot 1 = BOOT_EP (send)
    mp.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    mp.handles[2] = tick_w; // slot 4 = BOOT_TICK
    let pong_ok = rt::sys_spawn(BOOT_IMG_PONG, BOOT_MEM, &mp, sp.exit).is_ok();
    if !beta_ok || !pong_ok {
        tw(b"run: pong spawn failed\n");
    }
    wait_exits(sp, beta_ok as u64 + pong_ok as u64);
    // Release the per-run attenuated handles (the children hold their own copies).
    let _ = rt::sys_close(ep_recv);
    let _ = rt::sys_close(ep_send);
    let _ = rt::sys_close(tick_w);
}

/// `badgetest`: exercise the §14 badged-endpoint mint rules from the shell.
/// Phase 2 = the negative paths (the end-to-end delivery demo is added next).
fn badgetest(sp: &Spawner) {
    // Two distinct badges minted off our (unbadged, R_ATTENUATE-bearing) ep.
    let b7 = rt::sys_mint(sp.ep, 7, R_SEND);
    let b42 = rt::sys_mint(sp.ep, 42, R_SEND);
    match (b7, b42) {
        (Ok(_), Ok(_)) => tw(b"[sh] mint 7+42 ok\n"),
        _ => tw(b"[sh] !! mint failed\n"),
    }
    // Re-badging an already-badged cap is forbidden (unforgeability).
    if let Ok(b) = b7 {
        match rt::sys_mint(b, 99, R_SEND) {
            Err(SysError::Rights) => tw(b"[sh] re-badge denied ok\n"),
            _ => tw(b"[sh] !! re-badge NOT denied\n"),
        }
    }
    // Badge 0 is reserved for "unbadged".
    match rt::sys_mint(sp.ep, 0, R_SEND) {
        Err(SysError::Msg) => tw(b"[sh] badge 0 denied ok\n"),
        _ => tw(b"[sh] !! badge 0 NOT denied\n"),
    }
    // Amplification (a right the source lacks) is refused (law L5).
    match rt::sys_mint(sp.ep, 5, R_SEND | R_WRITE) {
        Err(SysError::Rights) => tw(b"[sh] amplify denied ok\n"),
        _ => tw(b"[sh] !! amplify NOT denied\n"),
    }
    // Minting a non-endpoint is a type error.
    match rt::sys_mint(BOOT_MEM, 7, 0) {
        Err(SysError::BadType) => tw(b"[sh] non-ep denied ok\n"),
        _ => tw(b"[sh] !! non-ep NOT denied\n"),
    }

    // --- end-to-end: spawn the badge server and prove delivery + unforgeability.
    let (b7, b42) = match (b7, b42) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return,
    };
    // The server receives on our endpoint; grant it the recv side at slot 1.
    let recv_cap = match rt::sys_attenuate(sp.ep, R_RECV | R_GRANT) {
        Ok(h) => h,
        Err(_) => return,
    };
    let mut m = MsgBuf::new(0);
    m.handle_count = 2;
    m.handles[0] = recv_cap; // slot 1 = BOOT_EP (recv)
    m.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    if rt::sys_spawn(BOOT_IMG_BADGE, BOOT_MEM, &m, sp.exit).is_ok() {
        // Send through each badged cap → server should report 7 then 42.
        let p = MsgBuf::new(0);
        let _ = rt::sys_send(b7, &p);
        let _ = rt::sys_send(b42, &p);
        // Forgery attempt: write a badge into the message and send via the
        // UNBADGED ep — the kernel overwrites it, so the server must report 0.
        let mut forged = MsgBuf::new(0);
        forged.badge = 1234;
        let _ = rt::sys_send(sp.ep, &forged);
        wait_exits(sp, 1);
    }
    let _ = rt::sys_close(recv_cap);
    let _ = rt::sys_close(b7);
    let _ = rt::sys_close(b42);
}

/// Strip leading and trailing spaces.
fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && s[a] == b' ' {
        a += 1;
    }
    while b > a && s[b - 1] == b' ' {
        b -= 1;
    }
    &s[a..b]
}

/// Pack a NUL-terminated name into a request MsgBuf's data.
fn pack_name(m: &mut MsgBuf, name: &[u8]) {
    let n = core::cmp::min(name.len(), 56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
}

/// WRITE `bytes` to the file `cap` starting at `start`, looping in <=48-byte
/// chunks. Returns the next write offset.
fn write_chunks(cap: Handle, bytes: &[u8], start: u64) -> u64 {
    let mut off = start;
    let mut i = 0;
    while i < bytes.len() {
        let n = core::cmp::min(48, bytes.len() - i);
        let mut wm = MsgBuf::new(TAG_FS_WRITE);
        wm.data[0] = off;
        wm.data[1] = n as u64;
        let dst = wm.data.as_mut_ptr() as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(bytes[i..].as_ptr(), dst.add(16), n) };
        wm.data_len = 8;
        if rt::sys_call(cap, &mut wm).is_err() {
            break;
        }
        let wrote = wm.data[0] as usize;
        if wrote == 0 {
            break; // out of space
        }
        off += wrote as u64;
        i += wrote;
    }
    off
}

/// `echo TEXT > FILE`: CREATE-or-truncate the file (relative to `dir`), write
/// TEXT + newline.
fn write_file(dir: Handle, name: &[u8], text: &[u8], append: bool) {
    if name.is_empty() {
        tw(b"sh: redirect needs a file name\n");
        return;
    }
    // Append mode: OPEN the file and write at its current end. If it doesn't
    // exist (or for '>'), CREATE-or-truncate and write at 0.
    let (cap, start) = if append {
        let mut o = MsgBuf::new(TAG_FS_OPEN);
        pack_name(&mut o, name);
        if rt::sys_call(dir, &mut o).is_ok()
            && o.data[0] == 0
            && o.data[1] == oxbow_abi::FS_FILE
        {
            (o.handles[0], o.data[2]) // existing file: append at its size
        } else {
            let mut c = MsgBuf::new(TAG_FS_CREATE);
            pack_name(&mut c, name);
            if rt::sys_call(dir, &mut c).is_err() || c.data[0] != 0 {
                tw(b"sh: cannot create ");
                tw(name);
                tw(b"\n");
                return;
            }
            (c.handles[0], 0)
        }
    } else {
        let mut c = MsgBuf::new(TAG_FS_CREATE);
        pack_name(&mut c, name);
        if rt::sys_call(dir, &mut c).is_err() || c.data[0] != 0 {
            tw(b"sh: cannot create ");
            tw(name);
            tw(b"\n");
            return;
        }
        (c.handles[0], 0)
    };
    let off = write_chunks(cap, text, start);
    let _ = write_chunks(cap, b"\n", off);
    let _ = rt::sys_close(cap);
}

/// `cd <name>` / `cd /`: change the current-directory capability. `cd` with no
/// arg (or `/`) returns to the root; `cd <name>` opens a subdir relative to the
/// current one. Confinement: there is no `cd ..` — you can't walk above a dir cap
/// you hold; `cd /` works only because the shell still holds the root cap.
fn cd(name: &[u8], cwd: &mut Handle, path: &mut Path) {
    // Normalize to a session-absolute target (handles `..`, `.`, absolute +
    // relative), then re-resolve it FROM SESSION_ROOT (§45) — the user's home
    // for non-root, the fs root for root. The fs rejects `..` past a cap and
    // collapses a leading `/` onto the cap's subtree, so this can never escape
    // the session root. `/` (and `cd` with no path) returns to the session root.
    let mut target = *path;
    target.apply(name);
    let commit = |cwd: &mut Handle, cap: Handle| {
        if *cwd != BOOT_FS_ROOT && *cwd != session_root() {
            let _ = rt::sys_close(*cwd);
        }
        *cwd = cap;
    };
    if target.len == 1 {
        commit(cwd, session_root());
        *path = target;
        return;
    }
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, target.as_bytes());
    if rt::sys_call(session_root(), &mut m).is_err() || m.data[0] != 0 {
        tw(b"cd: ");
        tw(name);
        tw(b": no such directory\n");
        return;
    }
    let cap = m.handles[0];
    if m.data[1] != oxbow_abi::FS_DIR {
        tw(b"cd: ");
        tw(name);
        tw(b": not a directory\n");
        let _ = rt::sys_close(cap);
        return;
    }
    commit(cwd, cap);
    *path = target;
}

/// Top-level line dispatch (§80): decide between the embedded Lua interpreter and
/// the bash-style command layer. Lua owns control flow / expressions / assignments;
/// the command layer owns program launch, builtins, redirects, and (next) pipes.
///   `= <expr>`   force Lua: evaluate and print an expression
///   `! <cmd>`    force the command layer (escape a name that looks like Lua)
///   otherwise    Lua if it looks like Lua, else a command
fn run(line: &[u8], sp: &Spawner, cwd: &mut Handle, path: &mut Path) {
    // Publish the live context so Lua's sh()/sh_out() can drive the command layer.
    unsafe {
        SHELL_CTX = ShellCtx { sp: sp as *const Spawner, cwd: cwd as *mut Handle, path: path as *mut Path };
    }
    let t = trim(line);
    if let Some(rest) = t.strip_prefix(b"=") {
        lua_eval(trim(rest));
        return;
    }
    if let Some(rest) = t.strip_prefix(b"!") {
        shell_run(trim(rest), sp, cwd, path);
        return;
    }
    if looks_like_lua(t) {
        lua_eval(t);
        return;
    }
    shell_run(t, sp, cwd, path);
}

/// The exit status of the last command the shell ran ($? in bash, §81). Set by
/// the spawn paths from the child's real exit code, and by builtins (0 = success,
/// 127 = not found). `&&`/`||` branch on it. Read with `last_status`.
static mut LAST_STATUS: i32 = 0;
fn set_status(s: i32) {
    unsafe { LAST_STATUS = s };
}
fn last_status() -> i32 {
    unsafe { LAST_STATUS }
}

/// A short-circuit conditional operator between commands.
#[derive(Clone, Copy)]
enum CondOp {
    And, // &&  run the next command only if the previous succeeded (status 0)
    Or,  // ||  run the next command only if the previous failed (status != 0)
}

/// Find the FIRST top-level `&&` or `||`, returning (before, op, after). A single
/// `|` (a pipe) is not an operator here — only the doubled form is. No quote
/// tracking yet.
fn split_cond(s: &[u8]) -> Option<(&[u8], CondOp, &[u8])> {
    let mut i = 0;
    while i + 1 < s.len() {
        if s[i] == b'&' && s[i + 1] == b'&' {
            return Some((&s[..i], CondOp::And, &s[i + 2..]));
        }
        if s[i] == b'|' && s[i + 1] == b'|' {
            return Some((&s[..i], CondOp::Or, &s[i + 2..]));
        }
        i += 1;
    }
    None
}

/// The bash command layer (§81): `;`-sequencing, then `&&`/`||` short-circuiting,
/// then `|`-pipelines (matching bash precedence: `;` < `&&`/`||` < `|`). Lua lines
/// never reach here — they're settled in `run` — so a `;`/`|`/`&&` inside a Lua
/// statement is the interpreter's, not the shell's.
fn shell_run(t: &[u8], sp: &Spawner, cwd: &mut Handle, path: &mut Path) {
    for seg in t.split(|&b| b == b';') {
        let seg = trim(seg);
        if !seg.is_empty() {
            run_conditional(seg, sp, cwd, path);
        }
    }
}

/// Evaluate one `&&`/`||` chain left to right, short-circuiting on `$?`. Each
/// command in the chain may itself be a `|` pipeline.
fn run_conditional(seg: &[u8], sp: &Spawner, cwd: &mut Handle, path: &mut Path) {
    let mut rest = seg;
    let mut op: Option<CondOp> = None; // operator BEFORE the current command (None = first)
    loop {
        let (cmd, next_op, remainder) = match split_cond(rest) {
            Some((c, o, r)) => (trim(c), Some(o), r),
            None => (trim(rest), None, &b""[..]),
        };
        let do_run = match op {
            None => true,
            Some(CondOp::And) => last_status() == 0,
            Some(CondOp::Or) => last_status() != 0,
        };
        if do_run {
            if cmd.is_empty() {
                set_status(0);
            } else {
                run_pipeline_or_cmd(cmd, sp, cwd, path);
            }
        }
        match next_op {
            None => break,
            Some(o) => {
                op = Some(o);
                rest = remainder;
            }
        }
    }
}

/// Run a single chain element: a `|` pipeline or a lone command. Both set $?.
fn run_pipeline_or_cmd(cmd: &[u8], sp: &Spawner, cwd: &mut Handle, path: &mut Path) {
    if cmd.iter().any(|&b| b == b'|') {
        run_pipeline(cmd, sp, cwd, path);
    } else {
        run_command(cmd, sp, cwd, path);
    }
}

/// Run a `|` pipeline (§81): wire each stage's stdout into the next stage's stdin
/// through a kernel pipe; the last stage writes to the tty. The pipe's EOF is
/// EXPLICIT (the kernel only ends a read once `sys_pipe_eof` marks the write side
/// closed), so the shell waits for each stage to exit IN ORDER and then EOFs that
/// stage's outgoing pipe — flushing all of a producer's bytes before its consumer
/// sees end-of-input. Stages are spawned right-to-left so a spawn failure can't
/// strand an already-running producer with no reader.
fn run_pipeline(seg: &[u8], sp: &Spawner, cwd: &mut Handle, path: &mut Path) {
    const MAXST: usize = 4;
    let mut stages: [&[u8]; MAXST] = [b""; MAXST];
    let mut nst = 0;
    for s in seg.split(|&b| b == b'|') {
        if nst >= MAXST {
            tw(b"sh: pipeline too long (max 4 stages)\n");
            return;
        }
        stages[nst] = trim(s);
        nst += 1;
    }
    for st in &stages[..nst] {
        if st.is_empty() {
            tw(b"sh: empty pipeline stage\n");
            return;
        }
    }
    if nst < 2 {
        run_command(stages[0], sp, cwd, path);
        return;
    }

    // One pipe between each pair of adjacent stages: wend[i]/rend[i] feed stage i+1.
    let mut wend = [HANDLE_NULL; MAXST];
    let mut rend = [HANDLE_NULL; MAXST];
    for i in 0..nst - 1 {
        let p = match rt::sys_pipe() {
            Ok(p) => p,
            Err(_) => {
                tw(b"sh: pipe creation failed\n");
                close_ends(&mut wend, &mut rend, MAXST);
                return;
            }
        };
        wend[i] = rt::sys_attenuate(p, R_OUT | R_GRANT).unwrap_or(HANDLE_NULL);
        rend[i] = rt::sys_attenuate(p, R_IN | R_GRANT).unwrap_or(HANDLE_NULL);
        let _ = rt::sys_close(p);
    }

    // Reuse the Spawner's per-stage exit notifications (never create/close per
    // pipeline — the kernel notif pool isn't freed on close, so that would leak).
    let exits = &sp.pexits;
    let mut spawned = [false; MAXST];
    // Right-to-left: consumers exist before their producers.
    for i in (0..nst).rev() {
        let stdin_h = if i > 0 { rend[i - 1] } else { HANDLE_NULL };
        let stdout_h = if i < nst - 1 { wend[i] } else { sp.stdout };
        spawned[i] = spawn_stage(stages[i], *cwd, path, sp, stdin_h, stdout_h, exits[i]);
        if !spawned[i] {
            break; // stop before launching upstream producers that would block
        }
    }
    // The children hold their own grants now; drop the shell's read-end copies.
    for i in 0..nst - 1 {
        if rend[i] != HANDLE_NULL {
            let _ = rt::sys_close(rend[i]);
        }
    }
    // Wait each stage out in order, EOFing its outgoing pipe once it has exited
    // (so the next stage drains the full output, then reads end-of-input).
    for i in 0..nst {
        if spawned[i] {
            let _ = rt::sys_notif_wait(exits[i]);
        }
        if i < nst - 1 && wend[i] != HANDLE_NULL {
            let _ = rt::sys_pipe_eof(wend[i]);
        }
    }
    // A pipeline's status is its LAST stage's exit code (bash semantics).
    set_status(if spawned[nst - 1] {
        rt::sys_notif_status(exits[nst - 1])
    } else {
        127
    });
    // Release the shell's write-end handles (the reusable exit notifs stay open).
    for i in 0..nst - 1 {
        if wend[i] != HANDLE_NULL {
            let _ = rt::sys_close(wend[i]);
        }
    }
}

/// Close every non-null pipe-end handle in the two arrays (cleanup helper).
fn close_ends(wend: &mut [Handle], rend: &mut [Handle], n: usize) {
    for i in 0..n {
        if wend[i] != HANDLE_NULL {
            let _ = rt::sys_close(wend[i]);
            wend[i] = HANDLE_NULL;
        }
        if rend[i] != HANDLE_NULL {
            let _ = rt::sys_close(rend[i]);
            rend[i] = HANDLE_NULL;
        }
    }
}

/// Spawn ONE pipeline stage WITHOUT waiting, granting it `stdin_h` at SPAWN_STDIN
/// (slot 4) and `stdout_h` at SPAWN_STDOUT (slot 2), with its own `exit` notifier.
/// Resolves the stage's verb to a boot image + slot-1 capability the way
/// `run_command` does. Returns true if the child was spawned. Only spawnable,
/// stream-friendly verbs are supported in a pipeline; builtins (echo/cd/…) and
/// unknowns are rejected.
fn spawn_stage(
    cmd0: &[u8],
    cwd: Handle,
    path: &Path,
    sp: &Spawner,
    stdin_h: Handle,
    stdout_h: Handle,
    exit: Handle,
) -> bool {
    let mut ebuf = [0u8; 512];
    let en = expand_line(cmd0, cwd, sp, &mut ebuf); // $VAR/quoting/glob per stage (§82)
    let cmd = &ebuf[..en];
    let (verb, rest) = split_cmd(cmd);
    // §94: a pipeline stage is just a /bin (or ~/bin / path) program FILE — the
    // same PATH resolution as a plain command. Each stage gets the cwd dir cap at
    // slot 1 and resolves its own path args; cat with no file arg reads stdin.
    let Some(len) = find_program(verb, path) else {
        tw(b"sh: ");
        tw(verb);
        tw(b": not found\n");
        return false;
    };
    let mut m = MsgBuf::new(0);
    m.data[0] = 0; // default budget
    m.data[1] = rest.as_ptr() as u64; // argv = rest (the program opens its own paths)
    m.data[2] = rest.len() as u64;
    m.data_len = 3;
    rt::msg_set_identity(&mut m, cur_ident());
    m.handle_count = 4;
    m.handles[0] = cwd; // slot 1 = cwd dir cap (ls/cat resolve names against it)
    m.handles[1] = stdout_h; // slot 2 = SPAWN_STDOUT (pipe write end or tty)
    m.handles[2] = stdin_h; // slot 4 = SPAWN_STDIN (pipe read end, or NULL)
    m.handles[3] = BOOT_NET_EP; // slot 20 = BOOT_NET_EP
    let elf = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(ELF_BUF) as *const u8, len) };
    let ok = rt::sys_spawn_bytes(elf, BOOT_MEM, &m, exit).is_ok();
    if !ok {
        tw(b"sh: pipeline stage spawn failed\n");
    }
    ok
}

fn run_command(line0: &[u8], sp: &Spawner, cwd: &mut Handle, path: &mut Path) {
    set_status(0); // default success; the spawn paths + the not-found arm override
    // Word-expansion (§82): $VAR/${VAR} from Lua globals, '…'/"…" quoting, then
    // `*` globbing. Done here, after operator splitting, so a `$var` holding
    // `|`/`;` is data, not syntax.
    let mut ebuf = [0u8; 512];
    let en = expand_line(line0, *cwd, sp, &mut ebuf);
    let line = &ebuf[..en];
    // Input redirect: `CMD < FILE` feeds FILE into CMD's stdin — desugar to the
    // pipeline `cat FILE | CMD`, reusing the pipe machinery. (Only stdin-reading
    // consumers like `cat -` actually consume it; others ignore the input.)
    if let Some(lt) = line.iter().position(|&b| b == b'<') {
        let cmdpart = trim(&line[..lt]);
        let (file, _) = split_cmd(trim(&line[lt + 1..]));
        if file.is_empty() {
            tw(b"sh: redirect needs a file name\n");
            set_status(1);
            return;
        }
        let mut buf = [0u8; 256];
        let mut n = 0;
        for part in [b"cat ".as_slice(), file, b" | ", cmdpart] {
            let take = core::cmp::min(part.len(), buf.len() - n);
            buf[n..n + take].copy_from_slice(&part[..take]);
            n += take;
        }
        run_pipeline(&buf[..n], sp, cwd, path);
        return;
    }
    // Output redirect: `echo TEXT > FILE` (truncate) or `>> FILE` (append).
    if let Some(gt) = line.iter().position(|&b| b == b'>') {
        let append = gt + 1 < line.len() && line[gt + 1] == b'>';
        let file_start = if append { gt + 2 } else { gt + 1 };
        let (cmd, text) = split_cmd(trim(&line[..gt]));
        let (file, _) = split_cmd(trim(&line[file_start..]));
        if cmd == b"echo" {
            write_file(*cwd, file, text, append);
        } else {
            tw(b"sh: only 'echo ... > file' redirect is supported\n");
        }
        return;
    }
    let (cmd, rest) = split_cmd(line);
    match cmd {
        b"" => {}
        b"echo" => {
            tw(rest);
            tw(b"\n");
        }
        b"run" => {
            let (prog, _) = split_cmd(rest);
            match prog {
                b"hello" => spawn_with(BOOT_IMG_HELLO, HANDLE_NULL, b"", sp),
                b"pong" => run_pong(sp),
                b"" => tw(b"run: usage: run <program>\n"),
                _ => tw(b"run: no such program\n"),
            }
        }
        // §94: ls/cat/mkdir/touch/rm/mv/cp are NOT builtins — they're files in
        // /bin, resolved by the PATH fallback below (delete /bin/ls and `ls` is
        // gone). cd/pwd stay builtins: they mutate the shell's own cwd state.
        b"cd" => cd(rest, cwd, path),
        b"pwd" => {
            tw(path.as_bytes());
            tw(b"\n");
        }
        b"dns" => dns_cmd(rest),
        b"http" => http_cmd(rest),
        b"drift" => spawn_with(BOOT_IMG_DRIFT, HANDLE_NULL, rest, sp),
        b"cc-hello" => spawn_with(BOOT_IMG_CCHELLO, *cwd, rest, sp),
        b"tcc" => spawn_with_budget(BOOT_IMG_TCC, *cwd, rest, 48 * 1024 * 1024, sp),
        b"cc" => cc_cmd(*cwd, rest, sp),
        b"lua" => spawn_with_budget(BOOT_IMG_LUA, *cwd, rest, 32 * 1024 * 1024, sp),
        b"py" | b"micropython" => spawn_with_budget(BOOT_IMG_UPY, *cwd, rest, 32 * 1024 * 1024, sp),
        b"js" | b"qjs" => spawn_with_budget(BOOT_IMG_QJS, *cwd, rest, 48 * 1024 * 1024, sp),
        b"curl" => spawn_with_budget(BOOT_IMG_CURL, *cwd, rest, 48 * 1024 * 1024, sp),
        b"cares-test" => spawn_with_budget(BOOT_IMG_CARES, HANDLE_NULL, rest, 48 * 1024 * 1024, sp),
        b"ffi-test" => spawn_with_budget(BOOT_IMG_FFI, HANDLE_NULL, rest, 16 * 1024 * 1024, sp),
        b"wl-test" => spawn_with_budget(BOOT_IMG_WL, HANDLE_NULL, rest, 16 * 1024 * 1024, sp),
        b"xkb-test" => spawn_with_budget(BOOT_IMG_XKB, HANDLE_NULL, rest, 16 * 1024 * 1024, sp),
        b"vterm-test" => spawn_with_budget(BOOT_IMG_VTERM, HANDLE_NULL, rest, 16 * 1024 * 1024, sp),
        b"ft-test" => spawn_with_budget(BOOT_IMG_FT, HANDLE_NULL, rest, 16 * 1024 * 1024, sp),
        b"exec" => exec_cmd(*cwd, path, rest, sp),
        b"sync" => sync_cmd(),
        b"chantest" => chantest(),
        b"rand" => rand_cmd(),
        b"fstest" => spawn_with_budget(BOOT_IMG_FSTEST, HANDLE_NULL, rest, 16 * 1024 * 1024, sp),
        b"jail" => jail_cmd(sp),
        b"badgetest" => badgetest(sp),
        b"whoami" => whoami_cmd(),
        b"id" => id_cmd(),
        b"groups" => groups_cmd(),
        b"su" => su_cmd(rest, cwd, path),
        b"passwd" => passwd_cmd(),
        b"logout" | b"exit" => {
            // §92: end the session and return to the graphical greeter — notify it
            // (it re-appears), then block for the next verified login.
            let _ = rt::channel::send(BOOT_SESSION_CHAN, b"L", &[]);
            session_gate(cwd, path);
        }
        b"help" => {
            tw(b"oxbow shell:  (ls cat mkdir touch are spawned programs)\n");
            tw(b"  echo <text>     print text (echo .. > f redirects to a file)\n");
            tw(b"  a | b           pipe a's output into b (e.g. ls | cat, cat f | cat)\n");
            tw(b"  a ; b           run a then b (command sequencing)\n");
            tw(b"  a && b / a || b run b on success / on failure of a (exit status)\n");
            tw(b"  cmd < file      feed file into cmd's stdin (e.g. cat - < readme.txt)\n");
            tw(b"  $VAR  ${VAR}    expand a variable (a shell var IS a Lua global)\n");
            tw(b"  $(cmd)          substitute a command's output; * globs; '..' \"..\" quote\n");
            tw(b"  lua: if/for/=   Lua control flow & expressions (= expr, ! forces a cmd)\n");
            tw(b"  sh\"cmd\"         from Lua: run a command (returns status); sh_out\"cmd\" captures\n");
            tw(b"  ls              list the current directory\n");
            tw(b"  cat <file>      print a file\n");
            tw(b"  mkdir <name>    make a directory\n");
            tw(b"  touch <name>    make an empty file\n");
            tw(b"  rm <name>       remove a file or empty dir\n");
            tw(b"  mv <old> <new>  rename within the directory\n");
            tw(b"  cp <src> <dst>  copy a file\n");
            tw(b"  cd <dir> | /    change directory (builtin)\n");
            tw(b"  dns <host>      resolve a hostname via the net UDP socket API\n");
            tw(b"  http <ip>       TCP GET / from <ip>:80 via the net socket API\n");
            tw(b"  drift           DRIFT crypto self-test (X25519/ChaCha20, needs SSE)\n");
            tw(b"  run hello/pong  spawn a demo program\n");
            tw(b"  cc <src> -o <o> compile+link a C file to a standalone binary (tcc -static)\n");
            tw(b"  lua [file.lua]  run the Lua 5.4 interpreter (built-in test, or a file)\n");
            tw(b"  py [file.py]    run MicroPython (built-in test, or a .py file)\n");
            tw(b"  js [file.js]    run QuickJS JavaScript (built-in test, or a .js file)\n");
            tw(b"  curl <url>      fetch an http:// URL (no TLS)\n");
            tw(b"  exec <path>     load + run an ELF from the filesystem (exec-from-fs)\n");
            tw(b"  sync            persist writable files to disk (restored at boot)\n");
            tw(b"  rand            print random bytes from the kernel CSPRNG (getentropy)\n");
            tw(b"  jail            confinement showcase: a hostile program is denied every escape\n");
            tw(b"  badgetest       exercise badged-endpoint mint rules\n");
            tw(b"  whoami          print the current user (capability-native identity)\n");
            tw(b"  id / groups     print uid/gid and group membership\n");
            tw(b"  su [user]       switch user (re-authenticate; default root)\n");
            tw(b"  passwd          change your password\n");
            tw(b"  logout          end the session and return to the login prompt\n");
            tw(b"  help            this list\n");
        }
        _ => {
            // §94: not a builtin — resolve it as a program on the filesystem
            // (/bin for system tools, ~/bin or an explicit path for user programs).
            if !path_exec(cmd, rest, *cwd, path, sp) {
                tw(b"oxbow: ");
                tw(cmd);
                tw(b": command not found\n");
                set_status(127);
            }
        }
    }
}

// ===========================================================================
// §44 — the login gate. Capability-native: authenticating proves who you are
// (blake2-hashed password) and, for non-root users, switches your cwd to your
// HOME-directory capability — the cap handoff. Identity is the shell's own
// mutable state (CUR_IDENT), propagated to every child it spawns. Authority is
// still the caps we hold; strict root-drop isolation is the next arc.
// ===========================================================================

/// A seeded account. `default_pw` is the plaintext we hash once at boot (the
/// known default — real systems seed from install config); after `passwd` lands
/// the stored hash no longer corresponds to anything in the binary.
struct AcctDef {
    name: &'static [u8],
    uid: u32,
    gid: u32,
    home: &'static [u8],
    groups: &'static [u32],
    default_pw: &'static [u8],
}

const ACCTS: &[AcctDef] = &[
    AcctDef { name: b"root", uid: 0, gid: 0, home: b"/", groups: &[0, 27], default_pw: b"root" },
    AcctDef {
        name: b"bryson",
        uid: 1000,
        gid: 1000,
        home: b"/home/bryson",
        groups: &[1000, 27],
        default_pw: b"bryson",
    },
];
const NACCT: usize = 2;

static mut SALTS: [[u8; 16]; NACCT] = [[0; 16]; NACCT];
static mut HASHES: [[u8; 32]; NACCT] = [[0; 32]; NACCT];
static mut SEEDED: bool = false;
/// The shell's current identity — set by the login gate, read by whoami/id and
/// stamped on every spawned child.
static mut CUR_IDENT: IdentRec = IdentRec::zeroed();

/// The capability that roots the current session's filesystem namespace (§45):
/// `BOOT_FS_ROOT` for root, the user's HOME dir cap for everyone else. The shell
/// resolves every user path — `cd`, `exec`, and the dir caps it hands to spawned
/// programs — FROM here, so a logged-in user is confined to their home subtree by
/// the filesystem (the fs collapses leading `/` onto the cap and rejects `..`).
/// The broad root cap is still held for the login machinery only (seeding /etc,
/// opening home dirs at auth) — never to resolve a session path.
static mut SESSION_ROOT: Handle = BOOT_FS_ROOT;

fn cur_ident() -> &'static IdentRec {
    unsafe { &*core::ptr::addr_of!(CUR_IDENT) }
}

fn session_root() -> Handle {
    unsafe { SESSION_ROOT }
}

/// Current login name, defaulting to "root" before/without a name.
fn cur_name() -> &'static [u8] {
    let n = cur_ident().name_bytes();
    if n.is_empty() {
        b"root"
    } else {
        n
    }
}

/// Salted, iterated blake2b — a real (if modest) password KDF. 4096 rounds.
fn hash_pw(salt: &[u8; 16], pw: &[u8]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    h.update(salt);
    h.update(pw);
    let mut out: [u8; 32] = h.finalize().into();
    for _ in 0..4096 {
        let mut h = Blake2b256::new();
        h.update(salt);
        h.update(&out);
        out = h.finalize().into();
    }
    out
}

/// Map a gid to a display name (for `id`); empty if unknown.
fn gid_name(gid: u32) -> &'static [u8] {
    match gid {
        0 => b"root",
        27 => b"wheel",
        100 => b"users",
        1000 => b"bryson",
        _ => b"",
    }
}

/// Adopt account `i`'s identity into CUR_IDENT.
fn set_ident(i: usize) {
    let a = &ACCTS[i];
    let mut id = IdentRec::new(a.uid, a.gid, a.name, a.home);
    for &g in a.groups {
        id.add_group(g);
    }
    unsafe {
        CUR_IDENT = id;
    }
}

/// Best-effort `mkdir` of an absolute path against the root cap (ignores
/// already-exists / errors — seeding is idempotent).
fn mkdir_one(path: &[u8]) {
    let mut m = MsgBuf::new(TAG_FS_MKDIR);
    pack_name(&mut m, path);
    let _ = rt::sys_call(BOOT_FS_ROOT, &mut m);
}

/// First-boot seed: random per-account salts + hashes, home directories, and a
/// human-readable /etc/passwd + /etc/group (cosmetic — auth uses the table).
fn seed_accounts() {
    unsafe {
        if SEEDED {
            return;
        }
        for (i, a) in ACCTS.iter().enumerate() {
            let mut salt = [0u8; 16];
            let _ = rt::sys_getentropy(&mut salt);
            SALTS[i] = salt;
            HASHES[i] = hash_pw(&salt, a.default_pw);
        }
        SEEDED = true;
    }
    mkdir_one(b"/home");
    mkdir_one(b"/home/bryson");
    mkdir_one(b"/etc");
    write_file(BOOT_FS_ROOT, b"/etc/passwd", b"root:x:0:0:/:/bin/sh\nbryson:x:1000:1000:/home/bryson:/bin/sh\n", false);
    write_file(BOOT_FS_ROOT, b"/etc/group", b"root:0:\nwheel:27:root,bryson\nbryson:1000:\n", false);
}

/// Establish account `i`'s session: SESSION_ROOT becomes their home capability
/// (the login cap handoff), cwd starts there, and their home is presented as `/`
/// — so they are confined to it (§45). root's session root is the fs root.
fn set_cwd_home(i: usize, cwd: &mut Handle, path: &mut Path) {
    let a = &ACCTS[i];
    // Release the previous session's home cap (but never the shared root cap).
    unsafe {
        if SESSION_ROOT != BOOT_FS_ROOT {
            let _ = rt::sys_close(SESSION_ROOT);
        }
    }
    if *cwd != BOOT_FS_ROOT && *cwd != session_root() {
        let _ = rt::sys_close(*cwd);
    }
    // Default to root's namespace; override below for users with a real home.
    unsafe {
        SESSION_ROOT = BOOT_FS_ROOT;
    }
    *path = Path::root();
    if a.home != b"/" {
        // Mint the home dir cap from the root authority (login machinery), then
        // adopt it as the session root — the only fs cap this session will use.
        let mut m = MsgBuf::new(TAG_FS_OPEN);
        pack_name(&mut m, a.home);
        if rt::sys_call(BOOT_FS_ROOT, &mut m).is_ok()
            && m.data[0] == 0
            && m.data[1] == oxbow_abi::FS_DIR
        {
            unsafe {
                SESSION_ROOT = m.handles[0];
            }
        }
    }
    *cwd = session_root();
}

/// Read a line as a password (echoes — the tty has no no-echo mode yet).
fn read_secret(line: &mut [u8; 256]) -> usize {
    tw(b"password: ");
    read_line(line)
}

/// Verify `name`+`pw` against the seeded credential store; returns the account
/// index on a match. The shell is the SOLE holder of the salts/hashes (§44/§92)
/// — both the tty login gate and the graphical session gate authenticate here.
fn verify_credentials(name: &[u8], pw: &[u8]) -> Option<usize> {
    seed_accounts();
    if let Some(i) = ACCTS.iter().position(|a| a.name == name) {
        let h = hash_pw(unsafe { &SALTS[i] }, pw);
        if h == unsafe { HASHES[i] } {
            return Some(i);
        }
    }
    None
}

/// Tell the tty to drop any buffered input (§92) — used after a graphical login
/// so the username/password the user typed into the greeter (which the kbd driver
/// also forwards to the tty) can't surface as phantom commands in the session.
fn tty_flush() {
    let m = MsgBuf::new(TAG_TTY_FLUSH);
    let _ = rt::sys_send(BOOT_TTY, &m);
}

/// §92 — the GRAPHICAL session gate. Instead of prompting on the tty, block on the
/// session channel for the compositor greeter's `username\npassword` relay,
/// verify it (we hold the credential store), and reply one byte: `1` ok / `0`
/// fail. On success adopt the identity + mint the HOME-directory capability (the
/// real authority that confines the session, §45); the greeter then dismisses and
/// the desktop appears. Re-entered by `logout`.
fn session_gate(cwd: &mut Handle, path: &mut Path) {
    seed_accounts();
    let mut buf = [0u8; 320];
    loop {
        let (n, _) = match rt::channel::recv(BOOT_SESSION_CHAN, &mut buf, &mut [], false) {
            Some(r) => r,
            None => continue, // EOF/error — retry rather than spin a bare session
        };
        if n == 0 {
            continue;
        }
        // Split the relay on its single newline: username \n password.
        let line = &buf[..n];
        let (name, pw): (&[u8], &[u8]) = match line.iter().position(|&b| b == b'\n') {
            Some(p) => (trim(&line[..p]), trim(&line[p + 1..])),
            None => (trim(line), b""),
        };
        if let Some(i) = verify_credentials(name, pw) {
            set_ident(i);
            set_cwd_home(i, cwd, path);
            let _ = rt::channel::send(BOOT_SESSION_CHAN, b"1", &[]);
            tty_flush(); // discard the greeter's keystrokes before the first prompt
            return;
        }
        let _ = rt::channel::send(BOOT_SESSION_CHAN, b"0", &[]);
    }
}

/// The tty login prompt loop (serial console / no-compositor fallback). Blocks
/// until a correct name+password, then sets the identity and home cwd. Retained
/// for headless boots; the graphical path uses [`session_gate`].
#[allow(dead_code)]
fn login_gate(cwd: &mut Handle, path: &mut Path) {
    let mut line = [0u8; 256];
    let mut namebuf = [0u8; 64];
    loop {
        tw(b"\nlogin: ");
        let n = read_line(&mut line);
        let nm = trim(&line[..n]);
        let nl = core::cmp::min(nm.len(), 64);
        namebuf[..nl].copy_from_slice(&nm[..nl]);
        let name = &namebuf[..nl];
        let sn = read_secret(&mut line);
        let pw = trim(&line[..sn]);
        if let Some(i) = verify_credentials(name, pw) {
            set_ident(i);
            set_cwd_home(i, cwd, path);
            tw(b"Welcome, ");
            tw(ACCTS[i].name);
            tw(b".\n");
            return;
        }
        tw(b"Login incorrect\n");
    }
}

/// `su [user]` (default root): re-authenticate and swap identity + home cwd.
fn su_cmd(arg: &[u8], cwd: &mut Handle, path: &mut Path) {
    let (name, _) = split_cmd(arg);
    let name = if name.is_empty() { b"root".as_slice() } else { name };
    if let Some(i) = ACCTS.iter().position(|a| a.name == name) {
        let mut line = [0u8; 256];
        let sn = read_secret(&mut line);
        let pw = trim(&line[..sn]);
        let h = hash_pw(unsafe { &SALTS[i] }, pw);
        if h == unsafe { HASHES[i] } {
            set_ident(i);
            set_cwd_home(i, cwd, path);
        } else {
            tw(b"su: Authentication failure\n");
        }
    } else {
        tw(b"su: unknown user: ");
        tw(name);
        tw(b"\n");
    }
}

/// `passwd`: change the current user's password (re-hash with a fresh salt).
/// In-memory only — the seeded defaults are restored at the next boot until the
/// credential store is persisted (next arc).
fn passwd_cmd() {
    let i = match ACCTS.iter().position(|a| a.name == cur_name()) {
        Some(i) => i,
        None => {
            tw(b"passwd: unknown user\n");
            return;
        }
    };
    let mut line = [0u8; 256];
    tw(b"current password: ");
    let n = read_line(&mut line);
    if hash_pw(unsafe { &SALTS[i] }, trim(&line[..n])) != unsafe { HASHES[i] } {
        tw(b"passwd: Authentication failure\n");
        return;
    }
    tw(b"new password: ");
    let n = read_line(&mut line);
    let np = trim(&line[..n]);
    let mut nb = [0u8; 128];
    let nl = core::cmp::min(np.len(), 128);
    nb[..nl].copy_from_slice(&np[..nl]);
    tw(b"retype new password: ");
    let n2 = read_line(&mut line);
    if trim(&line[..n2]) != &nb[..nl] {
        tw(b"passwd: passwords do not match\n");
        return;
    }
    let mut salt = [0u8; 16];
    let _ = rt::sys_getentropy(&mut salt);
    unsafe {
        SALTS[i] = salt;
        HASHES[i] = hash_pw(&salt, &nb[..nl]);
    }
    tw(b"passwd: updated (in-memory; resets to the default at reboot)\n");
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    tw(b"[sh] shell ready\n");
    // Negative self-check: the boot loader revoked our Console handle, so a
    // direct hardware write MUST fail. Prove it (reported through the tty).
    let probe = b"X";
    if rt::sys_console_write(BOOT_CONSOLE, probe.as_ptr(), 1).is_err() {
        tw(b"[sh] direct console write denied (revoked) ok\n");
    } else {
        tw(b"[sh] !! direct console write SUCCEEDED (revocation broken)\n");
    }
    // Mint the spawn capabilities once: an attenuated send-only "stdout" endpoint
    // to hand children (BOOT_TTY keeps R_GRANT so we can pass it on), and one exit
    // notification reused for every spawn.
    let sp = Spawner {
        stdout: rt::sys_attenuate(BOOT_TTY, R_SEND | R_GRANT).unwrap_or(HANDLE_NULL),
        exit: rt::sys_notif_create().unwrap_or(HANDLE_NULL),
        ep: rt::sys_ep_create().unwrap_or(HANDLE_NULL),
        pexits: [
            rt::sys_notif_create().unwrap_or(HANDLE_NULL),
            rt::sys_notif_create().unwrap_or(HANDLE_NULL),
            rt::sys_notif_create().unwrap_or(HANDLE_NULL),
            rt::sys_notif_create().unwrap_or(HANDLE_NULL),
        ],
    };
    // The current-directory capability + its path string (starts at the root).
    let mut cwd: Handle = BOOT_FS_ROOT;
    let mut path = Path::root();
    let mut line = [0u8; 256];
    // §44/§92: authenticate before the first prompt. The graphical greeter (in the
    // compositor) collects the credentials and relays them over the session
    // channel; session_gate verifies them here — the shell is the sole credential
    // authority — then adopts the identity + the user's home capability.
    session_gate(&mut cwd, &mut path);
    // §94: cache the /bin directory cap so bare command names resolve to system
    // tools on the filesystem (reachable by every user, independent of the
    // session's home confinement).
    open_bin_dir();
    loop {
        // user@oxbow path-aware prompt, e.g. `bryson@oxbow:/home/bryson$ `.
        tw(cur_name());
        tw(b"@oxbow:");
        tw(path.as_bytes());
        tw(b"$ ");
        let n = read_line(&mut line);
        run(&line[..n], &sp, &mut cwd, &mut path);
    }
}
