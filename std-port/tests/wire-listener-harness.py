import json,os,socket,subprocess,time
ROOT="/Users/5speeddeasil/oxbow";LOG="/tmp/pipe.log";PORT=45821
Q=["qemu-system-x86_64","-M","q35","-m","512M","-smp","4","-cdrom",ROOT+"/oxbow.iso","-boot","d","-serial",f"file:{LOG}","-display","none","-vga","none","-device","virtio-gpu-pci","-qmp",f"tcp:127.0.0.1:{PORT},server=on,wait=off","-no-reboot","-no-shutdown","-drive","file="+ROOT+"/oxbow-disk.img,if=none,id=disk0,format=raw","-device","virtio-blk-pci,drive=disk0","-netdev","user,id=net0,hostfwd=tcp:127.0.0.1:5555-:8080","-device","e1000,netdev=net0"]
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
client_result="(client not run)"
try:
 c=qmp(PORT); waitfor("compositor up",180); time.sleep(8)
 typ("root");key(c,"tab");typ("root");key(c,"ret"); waitfor("root@oxbow",30); time.sleep(3)
 cmd("pwd",None,4); base=len(ser())
 # launch the guest listener (don't wait for prompt; it blocks in accept)
 typ("oxtest");key(c,"ret")
 if waitfor("WIRELISTEN: listening",30):
  time.sleep(1)  # let the guest settle into accept()
  # connect from the Mac through slirp's hostfwd
  for attempt in range(8):
   try:
    cs=socket.create_connection(("127.0.0.1",5555),timeout=3)
    cs.sendall(b"PING\n")
    cs.settimeout(6)
    reply=cs.recv(64)
    cs.close()
    client_result=f"client got: {reply!r}"
    break
   except OSError as e:
    client_result=f"client error: {e}"
    time.sleep(1.5)
  waitfor("WIRELISTEN: replied",10)
 else:
  client_result="guest never reached 'listening'"
 s=ser()[base:]
 print("=== client:",client_result,"===")
 for l in s.splitlines():
  t=l.strip()
  if "WIRELISTEN" in t or "fault" in t or "panic" in t:
   print(t)
finally:
 try:c("quit")
 except:pass
 p.terminate()
 try:p.wait(timeout=10)
 except:p.kill()
