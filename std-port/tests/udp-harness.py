import json,os,socket,subprocess,time
ROOT="/Users/5speeddeasil/oxbow";LOG="/tmp/pipe.log";PORT=45819
Q=["qemu-system-x86_64","-M","q35","-m","512M","-smp","4","-cdrom",ROOT+"/oxbow.iso","-boot","d","-serial",f"file:{LOG}","-display","none","-vga","none","-device","virtio-gpu-pci","-qmp",f"tcp:127.0.0.1:{PORT},server=on,wait=off","-no-reboot","-no-shutdown","-drive","file="+ROOT+"/oxbow-disk.img,if=none,id=disk0,format=raw","-device","virtio-blk-pci,drive=disk0","-netdev","user,id=net0","-device","e1000,netdev=net0"]
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
def waitfor(marker,timeout):
 e=time.time()+timeout
 while time.time()<e:
  if marker in ser():return True
  time.sleep(0.3)
 return False
def key(c,q):
 c("input-send-event",events=[{"type":"key","data":{"down":True,"key":{"type":"qcode","data":q}}}]);time.sleep(0.03);c("input-send-event",events=[{"type":"key","data":{"down":False,"key":{"type":"qcode","data":q}}}]);time.sleep(0.13)
def typ(s):
 for ch in s:key(c,{" ":"spc",".":"dot","/":"slash","-":"minus","_":"shift_minus"}.get(ch,ch))
def waitfor_echo(s,t):
 e=time.time()+t
 while time.time()<e:
  if s in ser().rsplit("oxbow:/$ ",1)[-1]: return True
  time.sleep(0.2)
 return False
def cmd(s,donemark,tmo):
 for attempt in range(3):
  n=ser().count("oxbow:/$ ")
  typ(s);key(c,"ret")
  if waitfor_echo(s,4): break
  time.sleep(1)
 e=time.time()+tmo
 while time.time()<e:
  if (donemark and donemark in ser()) or ser().count("oxbow:/$ ")>n: return True
  time.sleep(0.3)
 return False
if os.path.exists(LOG):os.remove(LOG)
p=subprocess.Popen(Q,cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
try:
 c=qmp(PORT); waitfor("compositor up",180); time.sleep(8)
 typ("root");key(c,"tab");typ("root");key(c,"ret"); waitfor("root@oxbow",30); time.sleep(3)
 cmd("pwd",None,4); base=len(ser())
 ok=cmd("oxtest","test result:",300)
 s=ser()[base:]
 print("=== done-marker seen:",ok,"===")
 for l in s.splitlines():
  t=l.strip()
  if any(k in t for k in("running","test result","FAILED","panicked","fault","#UD","... ok","passed")):
   print(t)
finally:
 try:c("quit")
 except:pass
 p.terminate()
 try:p.wait(timeout=10)
 except:p.kill()
