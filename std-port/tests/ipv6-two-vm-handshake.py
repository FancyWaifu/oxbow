import json,os,socket,subprocess,time
ROOT="/Users/5speeddeasil/oxbow"

def base_q(serial, qmp_port, disk, netdev, mac):
    return ["qemu-system-x86_64","-M","q35","-m","512M","-smp","2","-cdrom",ROOT+"/oxbow.iso","-boot","d",
            "-serial",f"file:{serial}","-display","none","-vga","none","-device","virtio-gpu-pci",
            "-qmp",f"tcp:127.0.0.1:{qmp_port},server=on,wait=off","-no-reboot","-no-shutdown",
            "-drive",f"file={ROOT}/{disk},if=none,id=disk0,format=raw","-device","virtio-blk-pci,drive=disk0",
            "-netdev",netdev,"-device",f"e1000,netdev=net0,mac={mac}"]

class VM:
    def __init__(self, name, serial, qmp_port, disk, netdev, mac):
        self.name=name; self.serial=serial
        if os.path.exists(serial): os.remove(serial)
        self.p=subprocess.Popen(base_q(serial,qmp_port,disk,netdev,mac),cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        s=None
        for _ in range(200):
            try:s=socket.create_connection(("127.0.0.1",qmp_port),timeout=1);break
            except OSError:time.sleep(0.1)
        self.f=s.makefile("rwb");self.f.readline()
        self._c("qmp_capabilities")
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
        for ch in s:self.key({" ":"spc",".":"dot","/":"slash","-":"minus","_":"shift_minus",":":"shift_semicolon","[":"bracket_left","]":"bracket_right"}.get(ch,ch))
    def login(self):
        self.wait("compositor up",200); time.sleep(8)
        self.typ("root");self.key("tab");self.typ("root");self.key("ret")
        self.wait("root@oxbow",40); time.sleep(3)
        self.typ("pwd");self.key("ret");time.sleep(2)  # warmup (first command eaten)
    def run(self,cmd):
        self.typ(cmd);self.key("ret")
    def quit(self):
        try:self._c("quit")
        except:pass
        try:self.p.wait(timeout=10)
        except:self.p.kill()

a=None;b=None
try:
    # VM-A listens for VM-B's socket connection (must start first).
    a=VM("A","/tmp/pa.log",45901,"oxbow-disk.img","socket,id=net0,listen=:7799","52:54:00:00:00:0a")
    b=VM("B","/tmp/pb.log",45902,"oxbow-disk-b.img","socket,id=net0,connect=127.0.0.1:7799","52:54:00:00:00:0b")
    print("both VMs launched; booting...")
    a.login(); print("A: logged in")
    b.login(); print("B: logged in")
    a.run("oxtest listen")
    if a.wait("V6PEER: listening",30):
        print("A: listening; starting B connect")
        time.sleep(2)
        b.run("oxtest connect")
        b.wait("V6PEER: done",40)
        a.wait("V6PEER: done",15)
    else:
        print("A: never reached listening")
    print("=== VM-A (listener) ===")
    for l in a.ser().splitlines():
        if "V6PEER" in l: print("  ",l.strip())
    print("=== VM-B (connector) ===")
    for l in b.ser().splitlines():
        if "V6PEER" in l: print("  ",l.strip())
finally:
    if a:a.quit()
    if b:b.quit()
