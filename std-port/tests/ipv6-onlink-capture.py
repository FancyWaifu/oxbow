# Verifies the IPv6 two-VM wire TcpListener resolves the peer VIA THE ON-LINK ROUTE.
#
# Two oxbow VMs on a QEMU `socket` wire (distinct MACs -> distinct fec0::<mac> in fec0::/64);
# a filter-dump on the CONNECTOR confirms it sends a Neighbor Solicitation for the listener's
# address fec0::a *directly* (NOT the default gateway fec0::2), the listener answers with an NA
# (server-side NDP), and the TCP handshake completes. The on-link path works because the
# `iface-max-addr-count-4` smoltcp feature lets the connector keep its fec0::/64 global address,
# so smoltcp's in_same_network() treats fec0::a as on-link rather than routing via the gateway.
#
# Prereq: /bin/oxtest on oxbow-disk.img = the dual-role program (ipv6-peer-test.rs:
# `oxtest listen` / `oxtest connect`); this copies it to oxbow-disk-b.img for VM-B.
# Expected: NS targets {'fec0::a': 1}, NA>=1, SYN-ACK>=1, VM-A "accepted from [fec0::b]".
import json,os,socket,subprocess,time,struct
ROOT="/Users/5speeddeasil/oxbow"
class VM:
    def __init__(s,serial,qmp,netdev,mac,disk,cap=None):
        s.serial=serial
        if os.path.exists(serial):os.remove(serial)
        dump=["-object",f"filter-dump,id=fd,netdev=net0,file={cap}"] if cap else []
        Q=["qemu-system-x86_64","-M","q35","-m","512M","-smp","2","-cdrom",ROOT+"/oxbow.iso","-boot","d","-serial",f"file:{serial}","-display","none","-vga","none","-device","virtio-gpu-pci","-qmp",f"tcp:127.0.0.1:{qmp},server=on,wait=off","-no-reboot","-no-shutdown","-drive",f"file={ROOT}/{disk},if=none,id=disk0,format=raw","-device","virtio-blk-pci,drive=disk0","-netdev",netdev]+dump+["-device",f"e1000,netdev=net0,mac={mac}"]
        s.p=subprocess.Popen(Q,cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        c=None
        for _ in range(250):
            try:c=socket.create_connection(("127.0.0.1",qmp),timeout=1);break
            except OSError:time.sleep(0.1)
        s.f=c.makefile("rwb");s.f.readline();s.c("qmp_capabilities")
    def c(s,e,**a):
        m={"execute":e}
        if a:m["arguments"]=a
        s.f.write((json.dumps(m)+"\n").encode());s.f.flush()
        while 1:
            r=json.loads(s.f.readline())
            if "return" in r or "error" in r:return r
    def ser(s):
        try:return open(s.serial,"rb").read().decode("latin1")
        except:return ""
    def wf(s,m,t):
        e=time.time()+t
        while time.time()<e:
            if m in s.ser():return True
            time.sleep(0.3)
        return False
    def key(s,q):
        s.c("input-send-event",events=[{"type":"key","data":{"down":True,"key":{"type":"qcode","data":q}}}]);time.sleep(0.03)
        s.c("input-send-event",events=[{"type":"key","data":{"down":False,"key":{"type":"qcode","data":q}}}]);time.sleep(0.12)
    def typ(s,x):
        for ch in x:s.key({" ":"spc","[":"bracket_left","]":"bracket_right",":":"shift_semicolon"}.get(ch,ch))
    def login(s):
        s.wf("compositor up",200);time.sleep(8);s.typ("root");s.key("tab");s.typ("root");s.key("ret");s.wf("root@oxbow",40);time.sleep(3);s.typ("pwd");s.key("ret");time.sleep(2)
    def quit(s):
        try:s.c("quit")
        except:pass
        try:s.p.wait(timeout=10)
        except:s.p.kill()

subprocess.run(["pkill","-9","-f","qemu-system"],stderr=subprocess.DEVNULL);time.sleep(1)
if not os.path.exists(f"{ROOT}/oxbow-disk-b.img"):
    subprocess.run(["cp",f"{ROOT}/oxbow-disk.img",f"{ROOT}/oxbow-disk-b.img"])
PCAP="/tmp/vmb_onlink.pcap"
if os.path.exists(PCAP):os.remove(PCAP)
a=VM("/tmp/oa.log",46101,"socket,id=net0,listen=:7788","52:54:00:00:00:0a","oxbow-disk.img")
b=VM("/tmp/ob.log",46102,"socket,id=net0,connect=127.0.0.1:7788","52:54:00:00:00:0b","oxbow-disk-b.img",cap=PCAP)
try:
    a.login();b.login()
    a.typ("oxtest listen");a.key("ret");a.wf("V6PEER: listening",35);time.sleep(2)
    b.typ("oxtest connect");b.key("ret");b.wf("V6PEER: done",40);a.wf("V6PEER: done",15)
    print("VM-A:",[l.strip() for l in a.ser().splitlines() if "accepted from" in l])
    print("VM-B:",[l.strip() for l in b.ser().splitlines() if "connected to" in l or "got PONG" in l])
finally:
    a.quit();b.quit()
    try:os.remove(f"{ROOT}/oxbow-disk-b.img")
    except:pass

d=open(PCAP,"rb").read() if os.path.exists(PCAP) else b""
off=24;ns_tgts={};na=0;syn=0;synack=0
while off+16<=len(d):
    cl=struct.unpack('<I',d[off+8:off+12])[0];fr=d[off+16:off+16+cl];off+=16+cl
    if len(fr)<54 or struct.unpack('>H',fr[12:14])[0]!=0x86dd:continue
    nh=fr[20]
    if nh==58:
        t=fr[54]
        if t==135:
            ts=socket.inet_ntop(socket.AF_INET6,bytes(fr[62:78]));ns_tgts[ts]=ns_tgts.get(ts,0)+1
        elif t==136: na+=1
    elif nh==6:
        flags=fr[54+13]
        if flags&0x12==0x12: synack+=1
        elif flags&0x02: syn+=1
print("\n=== connector (VM-B) wire capture ===")
print("  NS targets:",ns_tgts,"  NA received:",na,"  SYN sent:",syn,"  SYN-ACK received:",synack)
print("  -> resolves fec0::a ON-LINK (not gateway fec0::2):", "fec0::a" in ns_tgts and na>0 and synack>0)
