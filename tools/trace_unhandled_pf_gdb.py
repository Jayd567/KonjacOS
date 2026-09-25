"""Optional companion for trace_jvm.py: record faults not serviced by vm."""
import gdb, json, pathlib
source=pathlib.Path(__file__).resolve().parents[1]/'kernel/src/paging.rs'
line=next(i for i,s in enumerate(source.read_text().splitlines(),1) if 'if f.cs & 3 == 3 && try_deliver_sigsegv(f)' in s)
class UnhandledFault(gdb.Breakpoint):
    def stop(self):
        try:
            f=gdb.selected_frame().read_var('f').dereference()
            print(json.dumps({'unhandled_pf': {k:hex(int(f[k])) for k in ['rip','rsp','cs','error_code']},
                              'cr2':hex(int(gdb.parse_and_eval('$cr2')) & ((1<<64)-1))}),flush=True)
        except gdb.error as e:
            print('PF diagnostic unavailable: '+str(e),flush=True)
        return False
UnhandledFault('paging.rs:'+str(line),type=gdb.BP_HARDWARE_BREAKPOINT)
