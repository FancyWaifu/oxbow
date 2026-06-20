import json,os,socket,subprocess,threading,time,struct
ROOT="/Users/5speeddeasil/oxbow"; DBG="/opt/homebrew/opt/e2fsprogs/sbin/debugfs"
results=[]

def kill_qemu():
    subprocess.run(["pkill","-9","-f","qemu-system"],stderr=subprocess.DEVNULL); time.sleep(1.5)

def inject(disk, binpath):
    subprocess.run([DBG,"-w","-R","rm /bin/oxtest",disk],stderr=subprocess.DEVNULL,stdout=subprocess.DEVNULL)
    subprocess.run([DBG,"-w","-R",f"write {binpath} /bin/oxtest",disk],stderr=subprocess.DEVNULL,stdout=subprocess.DEVNULL)

class VM:
    def __init__(self, serial, qmp, netdev, mac=None, disk="oxbow-disk.img", extra=None):
        self.serial=serial
        if os.path.exists(serial): os.remove(serial)
        dev=["-device","e1000,netdev=net0"] if not mac else ["-device",f"e1000,netdev=net0,mac={mac}"]
        Q=["qemu-system-x86_64","-M","q35","-m","512M","-smp","2","-cdrom",ROOT+"/oxbow.iso","-boot","d",
           "-serial",f"file:{serial}","-display","none","-vga","none","-device","virtio-gpu-pci",
           "-qmp",f"tcp:127.0.0.1:{qmp},server=on,wait=off","-no-reboot","-no-shutdown",
           "-drive",f"file={ROOT}/{disk},if=none,id=disk0,format=raw","-device","virtio-blk-pci,drive=disk0",
           "-netdev",netdev]+(extra or [])+dev
        self.p=subprocess.Popen(Q,cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        s=None
        for _ in range(250):
            try:s=socket.create_connection(("127.0.0.1",qmp),timeout=1);break
            except OSError:time.sleep(0.1)
        self.f=s.makefile("rwb");self.f.readline();self._c("qmp_capabilities")
    def _c(self,e,**a):
        m={"execute":e}
        if a:m["arguments"]=a
        self.f.write((json.dumps(m)+"\n").encode());self.f.flush()
        while 1:
            r=json.loads(self.f.readline())
            if "return" in r or "error" in r:return r
    def ser(self):
        try:return open(self.serial,"rb").read().decode("latin1")
        except:return ""
    def wait(self,m,t):
        e=time.time()+t
        while time.time()<e:
            if m in self.ser():return True
            time.sleep(0.3)
        return False
    def key(self,q):
        self._c("input-send-event",events=[{"type":"key","data":{"down":True,"key":{"type":"qcode","data":q}}}]);time.sleep(0.03)
        self._c("input-send-event",events=[{"type":"key","data":{"down":False,"key":{"type":"qcode","data":q}}}]);time.sleep(0.12)
    def typ(self,s):
        for ch in s:self.key({" ":"spc",".":"dot","/":"slash","-":"minus","_":"shift_minus","[":"bracket_left","]":"bracket_right",":":"shift_semicolon"}.get(ch,ch))
    def booted(self): return self.wait("compositor up",200)
    def login(self):
        time.sleep(8)
        self.typ("root");self.key("tab");self.typ("root");self.key("ret");self.wait("root@oxbow",40);time.sleep(3)
        self.typ("pwd");self.key("ret");time.sleep(2)
    def run(self,cmd): self.typ(cmd);self.key("ret")
    def run_until(self,cmd,marker,t):
        for _ in range(3):
            self.run(cmd)
            if self.wait(marker,t): return True
            time.sleep(1)
        return False
    def quit(self):
        try:self._c("quit")
        except:pass
        try:self.p.wait(timeout=10)
        except:self.p.kill()

def boot(serial, qmp, netdev, **kw):
    vm=None
    for attempt in range(3):
        kill_qemu()
        vm=VM(serial, qmp+attempt, netdev, **kw)
        if vm.booted(): vm.login(); return vm
        vm.quit()
    return vm

# ---------- TEST 1: loopback libtest (53 udp+tcp) ----------
def t_libtest():
    inject(f"{ROOT}/oxbow-disk.img","/tmp/bin_libtest")
    vm=boot("/tmp/s1.log",46001,"user,id=net0")
    try:
        vm.run_until("oxtest","test result:",320)
        line=[l for l in vm.ser().splitlines() if "test result:" in l]
        detail=line[-1].strip() if line else "(no result)"
        ok = "0 failed" in detail and "53 passed" in detail
        return ("loopback UDP+TCP libtest (53)", ok, detail)
    finally: vm.quit()

# ---------- TEST 2: DNS A+AAAA + large reply ----------
def t_dns():
    inject(f"{ROOT}/oxbow-disk.img","/tmp/bin_dns")
    vm=boot("/tmp/s2.log",46021,"user,id=net0")
    try:
        vm.run_until("oxtest","DNS: done",40)
        lines=[l.strip() for l in vm.ser().splitlines() if l.strip().startswith("DNS: ")]
        aaaa=sum(1 for l in lines if "(1 AAAA)" in l); a=sum(1 for l in lines if "->" in l)
        ok = a>=3 and aaaa>=3
        return ("DNS resolution (A + AAAA, real internet)", ok, f"{a} names resolved, {aaaa} with AAAA")
    finally: vm.quit()

# ---------- TEST 3: IPv4 wire TcpListener (hostfwd + host client) ----------
def t_wire4():
    inject(f"{ROOT}/oxbow-disk.img","/tmp/bin_wire4")
    vm=boot("/tmp/s3.log",46031,"user,id=net0,hostfwd=tcp:127.0.0.1:5570-:8080")
    try:
        if not vm.run_until("oxtest","WIRELISTEN: listening",40):
            return ("IPv4 wire TcpListener (inbound from host)", False, "never reached listening")
        time.sleep(1); reply=b""
        for _ in range(8):
            try:
                cs=socket.create_connection(("127.0.0.1",5570),timeout=3); cs.sendall(b"PING\n"); cs.settimeout(6)
                reply=cs.recv(64); cs.close(); break
            except OSError: time.sleep(1.5)
        vm.wait("WIRELISTEN: replied",10)
        ok = reply==b"PONG\n" and "WIRELISTEN: accepted" in vm.ser()
        return ("IPv4 wire TcpListener (inbound from host)", ok, f"host received {reply!r}")
    finally: vm.quit()

# ---------- TEST 4: IPv6 two-VM full handshake ----------
def t_ipv6_twovm():
    kill_qemu()
    inject(f"{ROOT}/oxbow-disk.img","/tmp/bin_v6peer")
    subprocess.run(["cp",f"{ROOT}/oxbow-disk.img",f"{ROOT}/oxbow-disk-b.img"])
    kill_qemu()
    a=None;b=None
    try:
        # Boot the LISTENER fully first (its socket netdev listens), then the connector —
        # booting both at once tends to stall, and sequential matches the socket roles.
        for at in range(3):
            if a: a.quit()
            a=VM("/tmp/s4a.log",46041+at,"socket,id=net0,listen=:7780",mac="52:54:00:00:00:0a",disk="oxbow-disk.img")
            if a.booted(): break
        a.login()
        if not a.run_until("oxtest listen","V6PEER: listening",35):
            return ("IPv6 two-VM full handshake (wire listener+connector)", False, "listener not up")
        for at in range(3):
            if b: b.quit()
            b=VM("/tmp/s4b.log",46045+at,"socket,id=net0,connect=127.0.0.1:7780",mac="52:54:00:00:00:0b",disk="oxbow-disk-b.img")
            if b.booted(): break
        b.login()
        b.run_until("oxtest connect","V6PEER: connecting",12)
        b.wait("V6PEER: done",40); a.wait("V6PEER: done",15)
        ok = "V6PEER: accepted from" in a.ser() and "connected to Ok" in b.ser() and "got PONG6" in b.ser()
        peer=[l.strip() for l in a.ser().splitlines() if "accepted from" in l]
        return ("IPv6 two-VM full handshake (wire listener+connector)", ok, peer[-1].replace("V6PEER: ","") if peer else "no accept")
    finally:
        if a: a.quit()
        if b: b.quit()
        try: os.remove(f"{ROOT}/oxbow-disk-b.img")
        except: pass

# ---------- TEST 5: dual-stack SLIRP coexistence (DHCP + DNS, ipv6 on) ----------
def t_dualstack():
    inject(f"{ROOT}/oxbow-disk.img","/tmp/bin_dns")
    vm=boot("/tmp/s5.log",46051,"user,id=net0,ipv4=on,ipv6=on")
    try:
        vm.run_until("oxtest","DNS: done",45)
        s=vm.ser()
        lease="[net] DHCP lease" in s; ready="[net] ready" in s; dns=sum(1 for l in s.splitlines() if l.strip().startswith("DNS: ") and "->" in l)
        ok = lease and ready and dns>=3
        return ("Dual-stack SLIRP (DHCP lease + DNS, ipv6=on)", ok, f"lease={lease} ready={ready} dns={dns}")
    finally: vm.quit()

print("=== oxbow net test suite — end to end ===\n")
for fn in [t_libtest, t_dns, t_wire4, t_ipv6_twovm, t_dualstack]:
    try: name,ok,detail = fn()
    except Exception as e: name,ok,detail = (fn.__name__, False, f"exception: {e}")
    results.append((name,ok,detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {name}\n        {detail}")
kill_qemu()
npass=sum(1 for _,ok,_ in results if ok)
print(f"\n=== SUITE: {npass}/{len(results)} categories passed ===")
