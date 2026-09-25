"""Read-only guest frame-pointer walk at an unhandled user page fault."""
import gdb,json,pathlib
source=pathlib.Path(__file__).resolve().parents[1]/'kernel/src/paging.rs'
line=next(i for i,s in enumerate(source.read_text().splitlines(),1) if 'if f.cs & 3 == 3 && try_deliver_sigsegv(f)' in s)
class StackFault(gdb.Breakpoint):
    def stop(self):
        try:
            f=gdb.selected_frame().read_var('f').dereference()
            row={k:hex(int(f[k])) for k in ['rip','rsp','rbp','cs','error_code']}
            row['cr2']=hex(int(gdb.parse_and_eval('$cr2'))&((1<<64)-1))
            frames=[]; fp=int(f['rbp']); lo=int(f['rsp']); seen=set()
            for _ in range(128):
                if fp in seen or fp<lo or fp>lo+2*1024*1024 or fp&7: break
                seen.add(fp)
                try: data=bytes(gdb.selected_inferior().read_memory(fp,16))
                except gdb.error: break
                parent=int.from_bytes(data[:8],'little'); pc=int.from_bytes(data[8:],'little')
                frames.append({'fp':hex(fp),'pc':hex(pc)})
                if parent<=fp: break
                fp=parent
            row['frames']=frames
            print(json.dumps({'java_stack_fault':row}),flush=True)
        except gdb.error as e: print('Stack diagnostic unavailable: '+str(e),flush=True)
        return False
StackFault('paging.rs:'+str(line),type=gdb.BP_HARDWARE_BREAKPOINT)
