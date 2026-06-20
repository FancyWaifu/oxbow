# End-to-end demo: boot oxbow, run /bin/httpd (a Rust std HTTP server), and fetch
# real pages from it over TCP from the Mac host via QEMU hostfwd.
import json, os, socket, subprocess, time
ROOT = "/Users/5speeddeasil/oxbow"
LOG = "/tmp/httpd.log"
QMP = 45920
HOSTPORT = 8088  # host side of the forward -> guest 8080

Q = ["qemu-system-x86_64", "-M", "q35", "-m", "512M", "-smp", "4",
     "-cdrom", ROOT + "/oxbow.iso", "-boot", "d",
     "-serial", f"file:{LOG}", "-display", "none", "-vga", "none",
     "-device", "virtio-gpu-pci",
     "-qmp", f"tcp:127.0.0.1:{QMP},server=on,wait=off",
     "-no-reboot", "-no-shutdown",
     "-drive", f"file={ROOT}/oxbow-disk.img,if=none,id=disk0,format=raw",
     "-device", "virtio-blk-pci,drive=disk0",
     "-netdev", f"user,id=net0,hostfwd=tcp:127.0.0.1:{HOSTPORT}-:8080",
     "-device", "e1000,netdev=net0"]

def qmp_conn(p):
    for _ in range(150):
        try:
            s = socket.create_connection(("127.0.0.1", p), timeout=1); break
        except OSError:
            time.sleep(0.1)
    f = s.makefile("rwb"); f.readline()
    def c(e, **a):
        m = {"execute": e}
        if a: m["arguments"] = a
        f.write((json.dumps(m) + "\n").encode()); f.flush()
        while 1:
            r = json.loads(f.readline())
            if "return" in r or "error" in r: return r
    c("qmp_capabilities"); return c

def ser():
    try: return open(LOG, "rb").read().decode("latin1")
    except: return ""

def waitfor(marker, timeout):
    e = time.time() + timeout
    while time.time() < e:
        if marker in ser(): return True
        time.sleep(0.3)
    return False

def key(c, q):
    c("input-send-event", events=[{"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": q}}}]); time.sleep(0.03)
    c("input-send-event", events=[{"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": q}}}]); time.sleep(0.13)

def typ(c, s):
    m = {" ": "spc", ".": "dot", "/": "slash", "-": "minus", "_": "shift_minus"}
    for ch in s: key(c, m.get(ch, ch))

def http_get(path, timeout=8):
    s = socket.create_connection(("127.0.0.1", HOSTPORT), timeout=timeout)
    s.sendall(f"GET {path} HTTP/1.0\r\nHost: oxbow\r\n\r\n".encode())
    s.settimeout(timeout)
    data = b""
    while True:
        try:
            chunk = s.recv(4096)
        except socket.timeout:
            break
        if not chunk: break
        data += chunk
    s.close()
    return data

if os.path.exists(LOG): os.remove(LOG)
subprocess.run(["pkill", "-9", "-f", "qemu-system"], stderr=subprocess.DEVNULL); time.sleep(1)
p = subprocess.Popen(Q, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
results = []
try:
    c = qmp_conn(QMP)
    if not waitfor("compositor up", 180):
        print("FAIL: never booted"); raise SystemExit
    time.sleep(8)
    # login root/root
    typ(c, "root"); key(c, "tab"); typ(c, "root"); key(c, "ret")
    waitfor("root@oxbow", 30); time.sleep(3)
    typ(c, "pwd"); key(c, "ret"); time.sleep(2)
    # launch the server
    typ(c, "httpd"); key(c, "ret")
    if not waitfor("serving", 30):
        print("FAIL: httpd did not start"); print(ser()[-800:]); raise SystemExit
    print("=== httpd started on oxbow ===")
    time.sleep(1)

    # --- real HTTP requests from the host ---
    def check(name, path, want_status, want_substr):
        try:
            resp = http_get(path)
        except OSError as e:
            results.append((name, False, f"conn error: {e}")); return
        head = resp.split(b"\r\n\r\n", 1)[0].decode("latin1", "replace")
        status_ok = f" {want_status} " in head.split("\r\n")[0]
        body_ok = want_substr.encode() in resp
        ok = status_ok and body_ok
        results.append((name, ok, head.split("\r\n")[0]))

    check("index (/)", "/", 200, "It works")
    check("about.txt", "/about.txt", 200, "pure Rust std")
    check("dir listing", "/files/", 200, "hello.txt")
    check("file in subdir", "/files/hello.txt", 200, "hello from oxbow")
    check("404 missing", "/nope.html", 404, "404")
    check("403 traversal", "/../etc/passwd", 403, "Forbidden")

    print("\n=== HTTP responses from oxbow's httpd (fetched over TCP from the host) ===")
    for name, ok, detail in results:
        print(f"[{'PASS' if ok else 'FAIL'}] {name:22} {detail}")
    npass = sum(1 for _, ok, _ in results if ok)
    print(f"\n=== {npass}/{len(results)} checks passed ===")

    print("\n=== full index.html body served by oxbow ===")
    body = http_get("/").split(b"\r\n\r\n", 1)[-1].decode("latin1", "replace")
    print(body[:600])

    print("\n=== guest httpd request log (serial) ===")
    for line in ser().splitlines():
        if "httpd:" in line: print("  " + line.strip())
finally:
    try: c("quit")
    except: pass
    try: p.wait(timeout=10)
    except: p.kill()
