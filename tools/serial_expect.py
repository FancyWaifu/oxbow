#!/usr/bin/env python3
"""Acceptance harness for the serial console: drive oxbow's shell entirely over
COM1-on-TCP. Proves the full path serial -> tty -> shell -> tty -> serial.

A cursor (`self.pos`) advances past each consumed match so a prompt is never
matched twice — without it, a stale `oxbow$ ` is matched and the next command is
sent before the real prompt prints, tripping the §11.4 type-ahead race.

Usage: serial_expect.py [port]   (default 45454).  Exit 0 on success, 1 on fail.
"""
import socket
import sys
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 45454
PROMPT = b"oxbow$ "


class Conn:
    def __init__(self, port):
        deadline = time.time() + 30
        self.sock = None
        while time.time() < deadline:
            try:
                self.sock = socket.create_connection(("127.0.0.1", port), timeout=2)
                break
            except OSError:
                time.sleep(0.3)
        if self.sock is None:
            raise SystemExit("FAIL: could not connect to QEMU serial socket")
        self.sock.settimeout(0.5)
        self.buf = b""
        self.pos = 0  # cursor: search only data at/after this offset

    def _recv(self):
        try:
            chunk = self.sock.recv(4096)
            if chunk:
                self.buf += chunk
                return True
        except socket.timeout:
            pass
        return False

    def read_until(self, needle, timeout=15.0):
        """Find `needle` at/after the cursor; on success move the cursor past it
        and return the segment from the old cursor to the match end."""
        end = time.time() + timeout
        while True:
            idx = self.buf.find(needle, self.pos)
            if idx != -1:
                seg = self.buf[self.pos: idx + len(needle)]
                self.pos = idx + len(needle)
                return seg
            if time.time() >= end:
                return None
            self._recv()

    def drain_quiet(self, quiet=1.2, maxwait=12.0):
        """Discard output until the line is quiet for `quiet`s (boot + the
        pong/beta demo have stopped), then park the cursor at the end."""
        end = time.time() + maxwait
        last = time.time()
        while time.time() < end:
            if self._recv():
                last = time.time()
            elif time.time() - last >= quiet:
                break
        self.pos = len(self.buf)

    def send(self, data):
        self.sock.sendall(data)


def main():
    c = Conn(PORT)
    fails = []

    if c.read_until(b"[serial] ready", 25) is None:
        fails.append("serial driver never readied")
    if c.read_until(PROMPT, 12) is None:
        fails.append("shell prompt never appeared")
    # The pong/beta demo runs on a ~1 Hz timer, so its output trails the prompt
    # by seconds. Wait for its final line before typing so it can't interleave.
    c.read_until(b"round 3 -> E_GONE ok", 12)
    c.drain_quiet()  # absorb the trailing [proc] exit lines; cursor to end

    def exchange(cmd, expect_output, reject=None):
        """Send `cmd`+CR; the echoed command line returns, then the output, then
        the next prompt. Assert the output appears (and `reject` does not)."""
        time.sleep(0.2)
        c.send(cmd + b"\r")
        seg = c.read_until(PROMPT, 8)
        if seg is None:
            fails.append(f"{cmd!r}: no prompt after command")
            return
        # Echo check: the printable prefix up to the first control byte must come
        # back literally (a DEL is echoed as a rub-out sequence, not as itself, so
        # we can't expect anything typed after it to appear verbatim).
        prefix = bytearray()
        for b in cmd:
            if 0x20 <= b <= 0x7E:
                prefix.append(b)
            else:
                break
        if prefix and bytes(prefix) not in seg:
            fails.append(f"{cmd!r}: command not echoed back (got {seg!r})")
        if expect_output not in seg:
            fails.append(f"{cmd!r}: missing output {expect_output!r} (got {seg!r})")
        if reject is not None and reject in seg:
            fails.append(f"{cmd!r}: rejected text {reject!r} present (got {seg!r})")

    # 1) echo: output 'hi' on its own line after the echoed command
    exchange(b"echo hi", b"\nhi\n")
    # 2) DEL editing: 'echo oZ' + DEL + 'k' -> output 'ok', never 'oZk'
    exchange(b"echo oZ\x7fk", b"\nok\n", reject=b"oZk")
    # 3) unknown command
    exchange(b"nope", b"nope: command not found")

    # --- cooked-mode echo (v1-cooked-tty): paste/type-ahead must NOT garble ---
    # These deliberately do NOT pace per character — eliminating the §12.5 race is
    # the point. A single contiguous-substring match is the strong check: with
    # cooked-mode echo every byte appears exactly once, in order.
    c.read_until(PROMPT, 8)
    # 4) two commands pasted at ONE prompt (the classic interleave case)
    c.send(b"echo a\recho b\r")
    if not c.read_until(b"echo a\na\noxbow$ echo b\nb\noxbow$ ", 8):
        fails.append("paste of two commands garbled")
    # 5) type-ahead DURING a long command buffers, then flushes clean afterwards
    c.send(b"run pong\r")
    time.sleep(0.3)
    c.send(b"echo after\r")  # lands mid-run (shell busy)
    c.read_until(b"round 3 -> E_GONE ok", 12)
    if not c.read_until(b"oxbow$ echo after\nafter\noxbow$ ", 10):
        fails.append("type-ahead during long command garbled")

    print("----- transcript -----")
    sys.stdout.write(c.buf.decode("latin1"))
    print("\n----- results -----")
    if fails:
        for f in fails:
            print(f"  FAIL: {f}")
        return 1
    print("  ALL PASS: echo, DEL-editing, unknown-command, paste + type-ahead")
    return 0


if __name__ == "__main__":
    sys.exit(main())
