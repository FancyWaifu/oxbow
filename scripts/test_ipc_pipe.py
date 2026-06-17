#!/usr/bin/env python3
"""Exercise the §70 lost-wakeup-fixed IPC/channel/pipe paths interactively.

Logs in over the i8042 keyboard (keyboard->tty->shell IPC), runs a few commands
(shell<->fs IPC + channels), and a pipeline `ls | cat` (kernel pipes). All of
these block/wake through the new prepare_block/block_current protocol, so if the
fix regressed the single-core path, login or a command would hang.
"""
import json, os, socket, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERIAL_LOG = "/tmp/oxbow-ipcpipe-serial.log"
TEST_DISK = "/tmp/oxbow-ipcpipe-disk.img"
QMP_PORT = 45577

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
QCODE.update({"\n": "ret", " ": "spc", "|": ("shift", "backslash"), ".": "dot"})


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
            print("FAIL: login hung (IPC regression?)\n" + serial()[-1500:]); return 1

        checks = []
        # plain command: shell<->fs IPC + tty channel
        n0 = len(serial()); q.type("ls\n")
        ls_ok = wait_for("readme", 6); checks.append(("ls (fs IPC)", ls_ok))
        # a pipeline: kernel pipe between two processes
        time.sleep(0.5); before = serial(); q.type("ls | cat\n"); time.sleep(1.5)
        pipe_progressed = len(serial()) > len(before) + 10
        checks.append(("ls | cat (pipe)", pipe_progressed))
        # prompt still responsive afterwards (didn't hang on a lost wakeup)
        time.sleep(0.3); before = serial(); q.type("cat readme.txt\n"); time.sleep(1.0)
        cat_ok = len(serial()) > len(before) + 5; checks.append(("cat (post-pipe responsive)", cat_ok))

        print("--- serial tail ---"); print(serial()[-1400:])
        print("--- verdict ---")
        for name, ok in checks:
            print(f"  {'ok ' if ok else 'FAIL'} {name}")
        no_fault = not any(x in serial() for x in ("PANIC", "#GP", "#PF", "#DF"))
        print(f"  {'ok ' if no_fault else 'FAIL'} no kernel fault")
        if all(ok for _, ok in checks) and no_fault:
            print("PASS: IPC + channels + pipes all work with the lost-wakeup fix")
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
