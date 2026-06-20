import json,os,socket,subprocess,time,struct
ROOT="/Users/5speeddeasil/oxbow";LOG="/tmp/pipe.log";PORT=45827;PCAP="/tmp/capdns.pcap"
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
 for ch in s:key(c,{" ":"spc",".":"dot","/":"slash","-":"minus","_":"shift_minus"}.get(ch,ch))
if os.path.exists(LOG):os.remove(LOG)
if os.path.exists(PCAP):os.remove(PCAP)
p=subprocess.Popen(Q,cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
try:
 c=qmp(PORT); waitfor("compositor up",180); time.sleep(8)
 typ("root");key(c,"tab");typ("root");key(c,"ret"); waitfor("root@oxbow",30); time.sleep(3)
 typ("pwd");key(c,"ret"); time.sleep(2)
 typ("oxtest");key(c,"ret")
 waitfor("DNS: done",40); time.sleep(2)
 for l in ser().splitlines():
  if "DNS:" in l: print(l.strip())
finally:
 try:c("quit")
 except:pass
 try:p.wait(timeout=15)
 except:p.kill()
 time.sleep(1)
# parse pcap: UDP datagrams with src port 53 = DNS responses; report payload size
data=open(PCAP,"rb").read() if os.path.exists(PCAP) else b""
off=24; resp=[]
while off+16<=len(data):
 caplen=struct.unpack('<I',data[off+8:off+12])[0]
 fr=data[off+16:off+16+caplen]; off+=16+caplen
 if len(fr)<14: continue
 if struct.unpack('>H',fr[12:14])[0]!=0x0800: continue  # IPv4
 ihl=(fr[14]&0x0f)*4; ipoff=14
 if fr[ipoff+9]!=17: continue  # UDP
 uoff=ipoff+ihl
 if uoff+8>len(fr): continue
 sport=struct.unpack('>H',fr[uoff:uoff+2])[0]
 ulen=struct.unpack('>H',fr[uoff+4:uoff+6])[0]
 if sport==53:
  resp.append(ulen-8)  # DNS message = UDP length minus 8-byte UDP header
print("=== DNS response sizes on the wire (UDP payload bytes) ===")
for i,n in enumerate(resp):
 print(f"  response {i+1}: {n} bytes" + ("   <-- LARGER than the old 56-byte inline cap" if n>56 else ""))
print(f"max response = {max(resp) if resp else 0} bytes; inline cap was 56")
