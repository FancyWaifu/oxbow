#!/usr/bin/env python3
"""Milestone 1 of the Lua-cored shell: prove the embedded Lua 5.4 interpreter
runs IN the shell process alongside the bash-style command layer.

Logs in over the i8042 keyboard, then types a mix of Lua and commands:
  - print + string + arithmetic (basic eval, output routed to the tty)
  - `= expr` explicit expression form
  - global persistence across separate REPL lines
  - single-line control flow (`for ... do ... end`)
  - a plain `ls` to confirm the command layer still works (no regression)
"""
import json, os, socket, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERIAL_LOG = "/tmp/oxbow-lua-serial.log"
TEST_DISK = "/tmp/oxbow-lua-disk.img"
QMP_PORT = 45590

QEMU = [
    "qemu-system-x86_64", "-M", "q35", "-m", "512M", "-smp", "4",
    "-cdrom", os.path.join(ROOT, "oxbow.iso"), "-boot", "d",
    "-serial", f"file:{SERIAL_LOG}", "-display", "none",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server=on,wait=off",
    "-no-reboot", "-no-shutdown",
    "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
    "-drive", "file=" + TEST_DISK + ",if=none,id=disk0,format=raw",
    "-device", "virtio-blk-pci,drive=disk0",
    "-netdev", "user,id=net0", "-device", "e1000,netdev=net0",
]

QCODE = {c: c for c in "abcdefghijklmnopqrstuvwxyz0123456789"}
QCODE.update({
    "\n": "ret", " ": "spc", ".": "dot", ",": "comma",
    "=": "equal", "/": "slash", ";": "semicolon",
    "(": ("shift", "9"), ")": ("shift", "0"),
    "+": ("shift", "equal"), "*": ("shift", "8"),
    '"': ("shift", "apostrophe"), "_": ("shift", "minus"),
    "!": ("shift", "1"), "|": ("shift", "backslash"),
    ">": ("shift", "dot"), "&": ("shift", "7"),
    "<": ("shift", "comma"), "-": "minus",
    "$": ("shift", "4"), "{": ("shift", "bracket_left"),
    "}": ("shift", "bracket_right"), "[": "bracket_left",
    "]": "bracket_right", "'": "apostrophe",
})


class Qmp:
    def __init__(self, port):
        for _ in range(100):
            try:
                self.s = socket.create_connection(("127.0.0.1", port), timeout=1); break
            except OSError:
                time.sleep(0.1)
        else:
            raise RuntimeError("QMP connect failed")
        self.f = self.s.makefile("rwb"); self._read(); self.cmd("qmp_capabilities")

    def _read(self):
        return json.loads(self.f.readline())

    def cmd(self, execute, **args):
        m = {"execute": execute}
        if args:
            m["arguments"] = args
        self.f.write((json.dumps(m) + "\n").encode()); self.f.flush()
        while True:
            r = self._read()
            if "return" in r or "error" in r:
                return r

    def _key1(self, qcode):
        self.cmd("input-send-event", events=[{"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": qcode}}}])
        self.cmd("input-send-event", events=[{"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": qcode}}}])

    def _key_shift(self, qcode):
        self.cmd("input-send-event", events=[{"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "shift"}}}])
        self._key1(qcode)
        self.cmd("input-send-event", events=[{"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "shift"}}}])

    def type(self, text):
        for c in text:
            if c.isupper():
                self._key_shift(c.lower())
                time.sleep(0.03)
                continue
            qc = QCODE[c]
            if isinstance(qc, tuple):
                self._key_shift(qc[1])
            else:
                self._key1(qc)
            time.sleep(0.03)


def serial():
    try:
        with open(SERIAL_LOG, "rb") as fh:
            return fh.read().decode("utf-8", "replace")
    except FileNotFoundError:
        return ""


def wait_for(needle, timeout):
    end = time.time() + timeout
    while time.time() < end:
        if needle in serial():
            return True
        time.sleep(0.2)
    return False


def main():
    if os.path.exists(SERIAL_LOG):
        os.remove(SERIAL_LOG)
    subprocess.run(["cp", os.path.join(ROOT, "oxbow-disk.img"), TEST_DISK], check=True)
    qemu = subprocess.Popen(QEMU, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        q = Qmp(QMP_PORT)
        if not wait_for("shell ready", 40):
            print("FAIL: never reached shell\n" + serial()[-1500:]); return 1
        time.sleep(2)
        q.type("bryson\n"); time.sleep(0.5); q.type("bryson\n")
        if not wait_for("Welcome", 8):
            print("FAIL: login hung\n" + serial()[-1500:]); return 1
        time.sleep(1)

        # Each line: (input, needle expected in the output afterwards)
        steps = [
            ('print("LUAMARK")\n', "LUAMARK"),       # string + print, tty routing
            ('print(111+222)\n', "333"),             # integer arithmetic
            ('= 6*7\n', "42"),                       # `=` explicit expression form
            ('sqv = 9\n', None),                     # global assignment (no output)
            ('print(sqv*sqv)\n', "81"),              # global persisted across lines
            ('for i=1,3 do print(i*100) end\n', "300"),  # single-line control flow
            ('ls\n', "readme"),                      # command layer still works
            # --- Milestone 2: pipes + sequencing (§81) ---
            ('ls | cat\n', "readme"),                # ls stdout -> pipe -> cat -> tty
            ('echo pipemark > pm.txt\n', None),      # set up a known file
            ('cat pm.txt | cat\n', "pipemark"),      # file -> pipe -> cat stdin -> tty
            ('echo seqone ; echo seqtwo\n', "seqtwo"),  # ';' sequencing (both run)
        ]
        checks = []
        for text, needle in steps:
            before = len(serial())
            q.type(text)
            if needle is None:
                time.sleep(0.6); checks.append((text.strip(), True)); continue
            ok = wait_for(needle, 6)
            checks.append((text.strip(), ok))
            time.sleep(0.3)

        # --- && / || short-circuit on exit status (§81) ---
        q.type("echo andA && echo andB\n")
        checks.append(("&& both run", wait_for("andB", 5) and "andA" in serial()))
        time.sleep(0.3)
        q.type("nocmd_xyz || echo orRECOV\n")
        checks.append(("|| runs after failure", wait_for("orRECOV", 5)))
        time.sleep(0.3)
        mark = len(serial())
        q.type("nocmd_xyz && echo andSKIP ; echo afterAND\n")
        ran = wait_for("afterAND", 5)
        # "andSKIP" appears once as the typed-command echo; if the skipped `echo`
        # had actually run it would appear a SECOND time as output. So count == 1.
        checks.append(("&& skips after failure", ran and serial()[mark:].count("andSKIP") == 1))
        time.sleep(0.3)
        # --- < stdin redirect (desugars to `cat pm.txt | cat -`) ---
        # "pipemark" is already in the buffer from earlier, so assert a NEW
        # occurrence appears (the typed command "cat - < pm.txt" contains none).
        before = serial().count("pipemark")
        q.type("cat - < pm.txt\n")
        lt_ok = False
        for _ in range(25):
            if serial().count("pipemark") > before:
                lt_ok = True
                break
            time.sleep(0.2)
        checks.append(("< stdin redirect", lt_ok))

        time.sleep(0.3)
        # --- $VAR / ${VAR} expansion (backed by Lua globals) + quoting (§82) ---
        # Output markers use brackets so they never collide with the command echo.
        q.type('gv = "zqv"\n')  # a shell var IS a Lua global
        time.sleep(0.5)
        q.type("echo [$gv]\n")
        checks.append(("$VAR expands", wait_for("[zqv]", 5)))
        time.sleep(0.3)
        q.type('echo "$gv-$gv"\n')
        checks.append(("$VAR in double quotes", wait_for("zqv-zqv", 5)))
        time.sleep(0.3)
        q.type("echo ${gv}TAIL\n")
        checks.append(("${VAR} braces", wait_for("zqvTAIL", 5)))
        time.sleep(0.3)
        mark = len(serial())
        q.type("echo '$gv'\n")  # single quotes: literal, no expansion
        time.sleep(1)
        checks.append(("'…' is literal", serial()[mark:].count("zqv") == 0))
        time.sleep(0.3)
        # --- globbing (§82) ---
        q.type("echo gg > guniq.txt\n")
        time.sleep(0.8)
        before = serial().count("guniq.txt")  # 1 (the command echo above)
        q.type("echo guni*\n")  # globs to guniq.txt (cmd echo has 'guni*', not the name)
        glob_ok = False
        for _ in range(25):
            if serial().count("guniq.txt") > before:
                glob_ok = True
                break
            time.sleep(0.2)
        checks.append(("* glob matches a file", glob_ok))
        time.sleep(0.3)
        mark = len(serial())
        q.type("echo NOMATCHQ*\n")  # no match -> stays literal
        time.sleep(1)
        checks.append(("* no-match stays literal", "NOMATCHQ*" in serial()[mark:]))
        time.sleep(0.3)
        # --- $(...) command substitution (§82) ---
        # Brackets make the OUTPUT marker absent from the command echo.
        q.type("echo [$(echo subval)]\n")  # echo builtin inside $()
        checks.append(("$() with echo", wait_for("[subval]", 5)))
        time.sleep(0.3)
        q.type("echo [$(cat pm.txt)]\n")  # real capture: cat's stdout via a pipe
        checks.append(("$() captures cmd stdout", wait_for("[pipemark]", 6)))
        time.sleep(0.3)
        # --- M4: Lua <-> shell interop (sh / sh_out) (§83) ---
        # sh() in a Lua loop: control flow drives shell commands. "lc2" is built
        # by concatenation, so it's absent from the command echo.
        q.type('for i=1,2 do sh("echo lc"..i) end\n')
        checks.append(("sh() from a Lua loop", wait_for("lc2", 6)))
        time.sleep(0.3)
        # sh_out() captures a command's stdout into a Lua string.
        q.type('v = sh_out("echo capval") ; print("<"..v..">")\n')
        checks.append(("sh_out() captures into Lua", wait_for("<capval>", 6)))
        time.sleep(0.3)
        # exit status is sh()'s return value.
        q.type('if sh("echo ok")==0 then print("STATZERO") end\n')
        checks.append(("sh() returns exit status", wait_for("STATZERO", 6)))

        print("--- serial tail ---"); print(serial()[-1600:])
        print("--- verdict ---")
        for name, ok in checks:
            print(f"  {'ok  ' if ok else 'FAIL'} {name!r}")
        no_fault = not any(x in serial() for x in ("PANIC", "#GP", "#PF", "#DF", "panic"))
        print(f"  {'ok  ' if no_fault else 'FAIL'} no kernel fault / lua panic")
        if all(ok for _, ok in checks) and no_fault:
            print("PASS: embedded Lua + command layer both work in-process")
            return 0
        print("FAIL"); return 1
    finally:
        try:
            q.cmd("quit")
        except Exception:
            pass
        qemu.terminate()
        try:
            qemu.wait(timeout=5)
        except Exception:
            qemu.kill()
        for f in (TEST_DISK, SERIAL_LOG):
            try:
                os.remove(f)
            except OSError:
                pass


if __name__ == "__main__":
    sys.exit(main())
