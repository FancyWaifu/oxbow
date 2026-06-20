import json,os,socket,subprocess,time
ROOT="/Users/5speeddeasil/oxbow";LOG="/tmp/pipe.log";PORT=45829
Q=["qemu-system-x86_64","-M","q35","-m","512M","-smp","4","-cdrom",ROOT+"/oxbow.iso","-boot","d","-serial",f"file:{LOG}","-display","none","-vga","none","-device","virtio-gpu-pci","-qmp",f"tcp:127.0.0.1:{PORT},server=on,wait=off","-no-reboot","-no-shutdown","-drive","file="+ROOT+"/oxbow-disk.img,if=none,id=disk0,format=raw","-device","virtio-blk-pci,drive=disk0","-netdev","user,id=net0,ipv4=on,ipv6=on","-device","e1000,netdev=net0"]
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
p=subprocess.Popen(Q,cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
try:
 c=qmp(PORT)
 waitfor("compositor up",180); time.sleep(8)
 typ("root");key(c,"tab");typ("root");key(c,"ret"); waitfor("root@oxbow",30); time.sleep(3)
 typ("pwd");key(c,"ret"); time.sleep(2)
 typ("oxtest");key(c,"ret")
 waitfor("DNS: done",40); time.sleep(2)
finally:
 try:c("quit")
 except:pass
 try:p.wait(timeout=15)
 except:p.kill()
print("=== net server + DNS under SLIRP ipv6=on ===")
for l in ser().splitlines():
 t=l.strip()
 if "[net] DHCP lease" in t or "[net] ready" in t or "DNS:" in t or "fallback" in t.lower():
  print(" ",t)
