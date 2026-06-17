#!/usr/bin/env python3
"""Headless reproduction of the keyboard+mouse i8042 wedge (§69 Phase 2c fix).

Boots oxbow under QEMU with QMP, then injects REAL PS/2 input through the i8042:
keystrokes (login) interleaved with mouse motion. Pre-fix, a keyboard byte
arriving while the IOAPIC line was masked left the shared output buffer wedged,
freezing both devices. We assert that keyboard input keeps registering AFTER
mouse motion by watching the serial console echo the typed commands.
"""
import json, os, socket, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERIAL_LOG = "/tmp/oxbow-kbdmouse-serial.log"
# A throwaway disk copy so this test can run alongside a live `run-tty` GUI
# session (which holds the write lock on the real oxbow-disk.img).
TEST_DISK = "/tmp/oxbow-kbdmouse-disk.img"
QMP_PORT = 45563

QEMU = [
    "qemu-system-x86_64", "-M", "q35", "-m", "512M", "-smp", "4",
    "-cdrom", os.path.join(ROOT, "oxbow.iso"), "-boot", "d",
    "-serial", f"file:{SERIAL_LOG}",
    "-display", "none",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server=on,wait=off",
    "-no-reboot", "-no-shutdown",
    "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
    "-drive", "file=" + TEST_DISK + ",if=none,id=disk0,format=raw",
    "-device", "virtio-blk-pci,drive=disk0",
]

# char -> QEMU qcode (enough for "bryson", "ls", "root")
QCODE = {c: c for c in "abcdefghijklmnopqrstuvwxyz0123456789"}
QCODE["\n"] = "ret"
QCODE[" "] = "spc"


class Qmp:
    def __init__(self, port):
        for _ in range(100):
            try:
                self.s = socket.create_connection(("127.0.0.1", port), timeout=1)
                break
            except OSError:
                time.sleep(0.1)
        else:
            raise RuntimeError("QMP connect failed")
        self.f = self.s.makefile("rwb")
        self._read()                      # greeting
        self.cmd("qmp_capabilities")

    def _read(self):
        return json.loads(self.f.readline())

    def cmd(self, execute, **args):
        msg = {"execute": execute}
        if args:
            msg["arguments"] = args
        self.f.write((json.dumps(msg) + "\n").encode())
        self.f.flush()
        while True:
            r = self._read()
            if "return" in r or "error" in r:
                return r

    def key(self, qcode):
        self.cmd("input-send-event", events=[
            {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": qcode}}}])
        self.cmd("input-send-event", events=[
            {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": qcode}}}])

    def type(self, text):
        for c in text:
            self.key(QCODE[c])
            time.sleep(0.03)

    def mouse_wiggle(self, n=20):
        for i in range(n):
            dx = 8 if i % 2 == 0 else -8
            self.cmd("input-send-event", events=[
                {"type": "rel", "data": {"axis": "x", "value": dx}},
                {"type": "rel", "data": {"axis": "y", "value": dx}}])
            time.sleep(0.02)


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
    qemu = subprocess.Popen(QEMU, cwd=ROOT,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        q = Qmp(QMP_PORT)
        # Boot far enough that the kbd driver is up and the login prompt is shown.
        if not wait_for("[kbd] ps/2 mouse enabled", 40):
            print("FAIL: kbd driver never came up\n--- serial tail ---")
            print(serial()[-2000:])
            return 1
        time.sleep(2)

        # 1) Move the mouse a lot BEFORE any keyboard — should work pre- and post-fix.
        q.mouse_wiggle(30)
        time.sleep(0.5)

        # 2) Log in via the i8042 keyboard (this is what used to wedge things).
        q.type("bryson\n")
        time.sleep(0.5)
        q.type("bryson\n")
        login_ok = wait_for("Welcome", 8)

        # 3) THE WEDGE TEST: alternate mouse motion and keyboard commands. If the
        #    i8042 buffer wedges, the keyboard stops echoing after the first motion.
        results = []
        for i, word in enumerate(["ls", "ls", "ls"]):
            q.mouse_wiggle(25)               # jam the shared buffer with mouse bytes
            time.sleep(0.2)
            before = serial()
            q.type(word + "\n")              # then type — must still register
            time.sleep(0.6)
            after = serial()
            # The shell echoes the command; count new prompt lines as progress.
            results.append(after.count("$ ") - before.count("$ ") + (len(after) - len(before)))

        log = serial()
        print("--- serial tail (last 1500 chars) ---")
        print(log[-1500:])
        print("--- verdict ---")
        print(f"login_ok={login_ok}")
        print(f"post-mouse keyboard progress per round = {results}")

        # PASS criteria: login succeeded AND every post-mouse keyboard round still
        # produced new serial output (keyboard not wedged by mouse traffic).
        if login_ok and all(r > 0 for r in results):
            print("PASS: keyboard kept working through interleaved mouse motion")
            return 0
        print("FAIL: keyboard appears to have wedged after mouse motion")
        return 1
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


if __name__ == "__main__":
    sys.exit(main())
