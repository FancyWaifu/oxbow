#!/usr/bin/env python3
"""Phase-2 probe: connect to oxbow's COM1-over-TCP, wait for boot, send a couple
of bytes, and confirm the serial driver reports them as `[serial] rx NN`.
This empirically proves IRQ4 delivery from the QEMU 16550 to the userspace driver.

Usage: serial_probe.py [port]   (default 45454)
Exit 0 on success, 1 on failure.
"""
import socket
import sys
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 45454


def read_until(sock, needle, timeout=20.0):
    sock.settimeout(0.5)
    buf = b""
    end = time.time() + timeout
    while time.time() < end:
        try:
            chunk = sock.recv(4096)
            if chunk:
                buf += chunk
                if needle in buf:
                    return buf
        except socket.timeout:
            pass
    return buf


def main():
    # QEMU (server=on,wait=on) is listening; connect.
    deadline = time.time() + 30
    sock = None
    while time.time() < deadline:
        try:
            sock = socket.create_connection(("127.0.0.1", PORT), timeout=2)
            break
        except OSError:
            time.sleep(0.3)
    if sock is None:
        print("FAIL: could not connect to QEMU serial socket")
        return 1

    boot = read_until(sock, b"[serial] ready", timeout=25)
    sys.stdout.write(boot.decode("latin1"))
    if b"[serial] ready" not in boot:
        print("\nFAIL: serial driver never announced ready")
        return 1
    # also confirm the shell prompt eventually shows (full stack alive)
    boot += read_until(sock, b"oxbow$ ", timeout=10)

    # Now type two bytes: 'a' (0x61) and 'b' (0x62).
    time.sleep(0.3)
    sock.sendall(b"ab")
    rx = read_until(sock, b"rx 62", timeout=8)
    sys.stdout.write(rx.decode("latin1"))

    ok_rights = b"io_out on R_IN port denied (E_RIGHTS) ok" in boot
    ok_a = b"rx 61" in rx or b"rx 61" in boot
    ok_b = b"rx 62" in rx
    print("\n--- results ---")
    print(f"  serial ready announced : {b'[serial] ready' in boot}")
    print(f"  R_IN write denied      : {ok_rights}")
    print(f"  rx 'a' (0x61) seen     : {ok_a}")
    print(f"  rx 'b' (0x62) seen     : {ok_b}")
    sock.close()
    return 0 if (ok_a and ok_b and ok_rights) else 1


if __name__ == "__main__":
    sys.exit(main())
