import json,os,socket,subprocess,threading,time,struct
from socket import inet_pton, AF_INET6
from scapy.all import Ether, IPv6, ICMPv6ND_NS, ICMPv6ND_NA, ICMPv6NDOptDstLLAddr, TCP, Raw
def same6(a,b):
    try:return inet_pton(AF_INET6,a)==inet_pton(AF_INET6,b)
    except Exception:return False

ROOT="/Users/5speeddeasil/oxbow";LOG="/tmp/pcon.log";QMP=45905;SOCK=7799
PEER_MAC="52:54:00:00:00:0a"; PEER_IP="fec0::a"     # the scapy peer (what oxbow connects to)
GUEST_MAC="52:54:00:00:00:0b"; GUEST_IP="fec0::b"   # the oxbow connector

state={"established":False,"got_data":None,"client_seq":0,"my_seq":4000}
log=[]

def serve():
    srv=socket.socket(); srv.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
    srv.bind(("127.0.0.1",SOCK)); srv.listen(1)
    conn,_=srv.accept()
    log.append("peer: QEMU connected to socket netdev")
    def send(pkt):
        b=bytes(pkt); conn.sendall(struct.pack(">I",len(b))+b)
    buf=b""
    while True:
        # read 4-byte length prefix + frame
        while len(buf)<4:
            d=conn.recv(65536)
            if not d: return
            buf+=d
        n=struct.unpack(">I",buf[:4])[0]; buf=buf[4:]
        while len(buf)<n:
            d=conn.recv(65536)
            if not d: return
            buf+=d
        frame=buf[:n]; buf=buf[n:]
        try: pkt=Ether(frame)
        except Exception: continue
        if ICMPv6ND_NS in pkt and len([x for x in log if x.startswith("rx-ns")])<3:
            log.append(f"rx-ns: target={pkt[ICMPv6ND_NS].tgt}")
        elif len(log)<22 and (TCP in pkt):
            log.append("rx-tcp: "+pkt.summary()[:80])
        if ICMPv6ND_NS in pkt and (same6(pkt[ICMPv6ND_NS].tgt, PEER_IP) or same6(pkt[ICMPv6ND_NS].tgt,"fec0::2")):
            # answer the NS — oxbow resolves the gateway fec0::2 (it routes fec0::a via
            # the default route), so claim that target with our MAC. R=1 marks us a router.
            tgt=pkt[ICMPv6ND_NS].tgt
            na=(Ether(src=PEER_MAC,dst=pkt[Ether].src)/
                IPv6(src=tgt,dst=pkt[IPv6].src)/
                ICMPv6ND_NA(tgt=tgt,R=1,S=1,O=1)/
                ICMPv6NDOptDstLLAddr(lladdr=PEER_MAC))
            send(na); log.append(f"peer: NA(tgt={tgt}) -> {pkt[IPv6].src}")
        elif TCP in pkt and pkt[IPv6].dst in (PEER_IP,"fec0:0:0:0:0:0:0:a") and pkt[TCP].dport==9090:
            t=pkt[TCP]
            if t.flags & 0x02 and not (t.flags & 0x10):  # SYN (no ACK)
                state["client_seq"]=t.seq
                sa=(Ether(src=PEER_MAC,dst=pkt[Ether].src)/IPv6(src=PEER_IP,dst=pkt[IPv6].src)/
                    TCP(sport=9090,dport=t.sport,flags="SA",seq=state["my_seq"],ack=t.seq+1,window=4096))
                send(sa); log.append("peer: SYN-ACK")
            elif t.flags & 0x01:  # FIN
                pass
            else:
                payload=bytes(t.payload)
                if payload:  # data (PING6) -> ack + send PONG6
                    state["established"]=True; state["got_data"]=payload
                    ack_no=t.seq+len(payload)
                    resp=(Ether(src=PEER_MAC,dst=pkt[Ether].src)/IPv6(src=PEER_IP,dst=pkt[IPv6].src)/
                          TCP(sport=9090,dport=t.sport,flags="PA",seq=state["my_seq"]+1,ack=ack_no,window=4096)/Raw(b"PONG6\n"))
                    send(resp); log.append(f"peer: got {payload!r}, sent PONG6")
                elif t.flags & 0x10 and not state["established"]:
                    state["established"]=True; log.append("peer: handshake ACK (established)")

def qmp_run():
    Q=["qemu-system-x86_64","-M","q35","-m","512M","-smp","2","-cdrom",ROOT+"/oxbow.iso","-boot","d","-serial",f"file:{LOG}","-display","none","-vga","none","-device","virtio-gpu-pci","-qmp",f"tcp:127.0.0.1:{QMP},server=on,wait=off","-no-reboot","-no-shutdown","-drive","file="+ROOT+"/oxbow-disk.img,if=none,id=disk0,format=raw","-device","virtio-blk-pci,drive=disk0","-netdev",f"socket,id=net0,connect=127.0.0.1:{SOCK}","-device",f"e1000,netdev=net0,mac={GUEST_MAC}"]
    if os.path.exists(LOG):os.remove(LOG)
    p=subprocess.Popen(Q,cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    s=None
    for _ in range(200):
        try:s=socket.create_connection(("127.0.0.1",QMP),timeout=1);break
        except OSError:time.sleep(0.1)
    f=s.makefile("rwb");f.readline()
    def c(e,**a):
        m={"execute":e}
        if a:m["arguments"]=a
        f.write((json.dumps(m)+"\n").encode());f.flush()
        while 1:
            r=json.loads(f.readline())
            if "return" in r or "error" in r:return r
    c("qmp_capabilities")
    def ser():
        try:return open(LOG,"rb").read().decode("latin1")
        except:return ""
    def wf(m,t):
        e=time.time()+t
        while time.time()<e:
            if m in ser():return True
            time.sleep(0.3)
        return False
    def key(q):
        c("input-send-event",events=[{"type":"key","data":{"down":True,"key":{"type":"qcode","data":q}}}]);time.sleep(0.03)
        c("input-send-event",events=[{"type":"key","data":{"down":False,"key":{"type":"qcode","data":q}}}]);time.sleep(0.12)
    def typ(s):
        for ch in s:key({" ":"spc",".":"dot","/":"slash","-":"minus","_":"shift_minus"}.get(ch,ch))
    wf("compositor up",200);time.sleep(8)
    typ("root");key("tab");typ("root");key("ret");wf("root@oxbow",40);time.sleep(3)
    typ("pwd");key("ret");time.sleep(2)
    typ("oxtest connect");key("ret")
    wf("V6PEER: done",40);time.sleep(2)
    print("=== oxbow connector (client) ===")
    for l in ser().splitlines():
        if "V6PEER" in l: print("  ",l.strip())
    try:c("quit")
    except:pass
    try:p.wait(timeout=10)
    except:p.kill()

t=threading.Thread(target=serve,daemon=True);t.start()
time.sleep(1)
qmp_run()
print("=== scapy peer log ===")
for l in log: print("  ",l)
print("=== handshake established:",state["established"],"  data:",state["got_data"])
