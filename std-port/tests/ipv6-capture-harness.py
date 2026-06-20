import json,os,socket,subprocess,time,struct
ROOT="/Users/5speeddeasil/oxbow";LOG="/tmp/pipe.log";PORT=45825;PCAP="/tmp/cap6.pcap"
Q=["qemu-system-x86_64","-M","q35","-m","512M","-smp","4","-cdrom",ROOT+"/oxbow.iso","-boot","d","-serial",f"file:{LOG}","-display","none","-vga","none","-device","virtio-gpu-pci","-qmp",f"tcp:127.0.0.1:{PORT},server=on,wait=off","-no-reboot","-no-shutdown","-drive","file="+ROOT+"/oxbow-disk.img,if=none,id=disk0,format=raw","-device","virtio-blk-pci,drive=disk0","-netdev","user,id=net0","-object",f"filter-dump,id=f0,netdev=net0,file={PCAP}","-device","e1000,netdev=net0"]
def qmp(p):
 for _ in range(150):
  try:s=socket.create_connection(("127.0.0.1",p),timeout=1);break
  except OSError:time.sleep(0.1)
 f=s.makefile("rwb");f.readline()
 def c(e,**a):
  m={"execute":e}
  if a:m["arguments"]=a
  f.write((json.dumps(m)+"\n").encode());f.flush()
  while 1:
   r=json.loads(f.readline())
   if "return" in r or "error" in r:return r
 c("qmp_capabilities");return c
def ser():
 try:return open(LOG,"rb").read().decode("latin1")
 except:return ""
def waitfor(m,t):
 e=time.time()+t
 while time.time()<e:
  if m in ser():return True
  time.sleep(0.3)
 return False
def key(c,q):
 c("input-send-event",events=[{"type":"key","data":{"down":True,"key":{"type":"qcode","data":q}}}]);time.sleep(0.03);c("input-send-event",events=[{"type":"key","data":{"down":False,"key":{"type":"qcode","data":q}}}]);time.sleep(0.13)
def typ(s):
 for ch in s:key(c,{" ":"spc",".":"dot","/":"slash","-":"minus","_":"shift_minus",":":"shift_semicolon","[":"bracket_left","]":"bracket_right"}.get(ch,ch))
if os.path.exists(LOG):os.remove(LOG)
if os.path.exists(PCAP):os.remove(PCAP)
p=subprocess.Popen(Q,cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
try:
 c=qmp(PORT); waitfor("compositor up",180); time.sleep(8)
 typ("root");key(c,"tab");typ("root");key(c,"ret"); waitfor("root@oxbow",30); time.sleep(3)
 typ("pwd");key(c,"ret"); time.sleep(2)
 typ("oxtest");key(c,"ret")
 waitfor("WIRE6: done",30); time.sleep(2)
 print("=== guest serial ===")
 for l in ser().splitlines():
  if "WIRE6" in l: print(l.strip())
finally:
 try:c("quit")
 except:pass
 try:p.wait(timeout=15)
 except:p.kill()
 time.sleep(1)
# analyze pcap for IPv6 frames from the guest
data=open(PCAP,"rb").read() if os.path.exists(PCAP) else b""
off=24; v6=0; total=0; samples=[]
GUEST_MAC="52:54:00:12:34:56"
while off+16<=len(data):
 caplen=struct.unpack('<I',data[off+8:off+12])[0]
 frame=data[off+16:off+16+caplen]; off+=16+caplen; total+=1
 if len(frame)>=14:
  et=struct.unpack('>H',frame[12:14])[0]
  if et==0x86dd:
   v6+=1
   src=':'.join('%02x'%b for b in frame[6:12])
   nh=frame[20] if len(frame)>=21 else 0
   if src==GUEST_MAC and len(samples)<8:
    icmp6t=frame[54] if nh==58 and len(frame)>54 else None
    nd={133:'RouterSolicit',135:'NeighborSolicit',136:'NeighborAdvert',128:'EchoReq'}.get(icmp6t,'')
    kind={58:f'ICMPv6 {nd}',6:'TCP-SYN?',17:'UDP'}.get(nh,f'nh={nh}')
    samples.append((src,kind,len(frame)))
print(f"=== pcap: total frames={total}  IPv6 frames={v6} ===")
print(f"IPv6 frames FROM guest ({GUEST_MAC}):")
for s,k,l in samples: print(f"  {k} len={l}")
