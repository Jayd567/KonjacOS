"""Bounded single-VM syscall trace. Run under WSL from the repo root."""
import subprocess,socket,json,time,pathlib,sys,signal,os,shutil
ROOT=pathlib.Path(__file__).resolve().parents[1]
def send_text(s, f, text):
    aliases = {' ': 'spc', '\n': 'ret', '=': 'equal', '.': 'dot', '/': 'slash', '-': 'minus', ',': 'comma', '_': 'minus'}
    for ch in text:
        keys = ([{'type': 'qcode', 'data': 'shift'}] if ch.isupper() or ch == '_' else [])
        keys.append({'type': 'qcode', 'data': aliases.get(ch, ch.lower())})
        s.sendall(json.dumps({'execute': 'send-key', 'arguments': {'keys': keys}}).encode()+b'\n')
        while True:
            reply = json.loads(f.readline())
            if 'error' in reply: raise RuntimeError(reply)
            if 'return' in reply: break
        time.sleep(.03)
label=sys.argv[1] if len(sys.argv)>1 else 'classpath-before'
command=sys.argv[2] if len(sys.argv)>2 else 'run /usr/lib/jvm/java-21-openjdk-amd64/bin/java'
seconds=int(sys.argv[3]) if len(sys.argv)>3 else 130
out=pathlib.Path('/tmp/konjac-'+label); out.mkdir(exist_ok=True)
extra = ('source '+os.environ['KONJAC_GDB_EXTRA']+'\n') if os.environ.get('KONJAC_GDB_EXTRA') else ''
trace_enabled=os.environ.get('KONJAC_TRACE_SYSCALLS','1')!='0'
trace_setup=('source '+str(ROOT/'tools/jvm_syscall_trace_gdb.py')+'\n') if trace_enabled else ''
pending_command='python show_pending()\n' if trace_enabled else ''
script='set pagination off\nset confirm off\nset language c\nfile '+str(ROOT/'kernel/target/x86_64-unknown-linux-gnu/debug/kernel')+'\ntarget remote localhost:1235\n'+trace_setup+extra+'continue\n'+pending_command+'bt\ninfo registers rip rsp cr2 cr3\ndetach\nquit\n'
(out/'trace.gdb').write_text(script)
(out/'serial.log').write_text('')
firmware = ['-drive','if=pflash,format=raw,unit=0,file=/usr/share/OVMF/OVMF_CODE_4M.fd,readonly=on',
            '-drive','if=pflash,format=raw,unit=1,file=/usr/share/OVMF/OVMF_VARS_4M.fd,snapshot=on'] if os.environ.get('KONJAC_UEFI') else []
q=subprocess.Popen(['qemu-system-x86_64','-m','256M','-display','none','-serial','file:'+str(out/'serial.log'),'-no-reboot','-no-shutdown','-boot','order=d','-cdrom',str(ROOT/'image.iso'),'-drive','file='+str(ROOT/'disk.img')+',format=raw,if=ide,index=0,media=disk,snapshot=on','-qmp','unix:'+str(out/'qmp.sock')+',server=on,wait=off','-gdb','tcp:127.0.0.1:1235']+firmware)
g=None
try:
 deadline=time.monotonic()+40
 while time.monotonic()<deadline:
  if q.poll() is not None: raise RuntimeError('QEMU exited')
  if (out/'serial.log').exists() and 'handing off to the shell' in (out/'serial.log').read_text(): break
  time.sleep(.25)
 else: raise RuntimeError('boot timeout')
 with (out/'trace.log').open('w') as log:
  g=subprocess.Popen(['gdb','-q','-batch','-x',str(out/'trace.gdb')],stdout=log,stderr=subprocess.STDOUT)
  time.sleep(2)
  s=socket.socket(socket.AF_UNIX); s.settimeout(10); s.connect(str(out/'qmp.sock')); f=s.makefile('rwb'); f.readline()
  s.sendall(b'{"execute":"qmp_capabilities"}\n'); f.readline()
  send_text(s,f,command+'\n')
  after_fault=os.environ.get('KONJAC_AFTER_FAULT')
  fault_observed=False
  deadline=time.monotonic()+seconds
  while time.monotonic()<deadline:
   time.sleep(.5)
   trace=(out/'trace.log').read_text()
   serial=(out/'serial.log').read_text()
   if after_fault and not fault_observed and 'ring 3 (CS=0x4b) -- killing task' in serial:
    time.sleep(1)
    send_text(s,f,after_fault+'\n')
    fault_observed=True
   if '*** KERNEL PANIC ***' in serial or ('*** CPU EXCEPTION:' in serial and not after_fault) or 'Failed setting boot class path.' in trace or '"text": "PASS' in trace or '"text": "FAIL' in trace or 'hello from glibc dynamic' in trace:
    time.sleep(2); break
  g.send_signal(signal.SIGINT)
  g.wait(timeout=30)
  s.sendall(json.dumps({'execute':'screendump','arguments':{'filename':str(out/'screen.ppm')}}).encode()+b'\n')
  while True:
   reply=json.loads(f.readline())
   if 'return' in reply or 'error' in reply: break
  from PIL import Image
  Image.open(out/'screen.ppm').save(ROOT/('trace-'+label+'.png'))
  s.close()
 print('Trace:',out/'trace.log')
 print('\n'.join((out/'trace.log').read_text().splitlines()[-45:]))
finally:
 try:
  if g and g.poll() is None:
   g.terminate()
   try: g.wait(timeout=5)
   except subprocess.TimeoutExpired: g.kill(); g.wait()
 finally:
  q.terminate()
  try: q.wait(timeout=5)
  except subprocess.TimeoutExpired: q.kill(); q.wait()
  # Preserve evidence even if debugger shutdown or screenshot capture failed.
  saved=ROOT/'.work'/'traces'/label; saved.mkdir(parents=True,exist_ok=True)
  for name in ['trace.log','serial.log','trace.gdb']:
   if (out/name).exists(): shutil.copyfile(out/name,saved/name)
