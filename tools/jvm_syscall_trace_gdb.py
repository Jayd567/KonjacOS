import gdb, json
pending = {}

def user_context(frame):
    words=bytes(gdb.selected_inferior().read_memory(frame,128))
    slots=[int.from_bytes(words[i:i+8],'little') for i in range(0,128,8)]
    fp,sp=slots[7],slots[15]
    result={'rip':hex(slots[13]),'rsp':hex(sp),'frames':[]}
    for _ in range(16):
        if fp<sp or fp>sp+2*1024*1024 or fp&7: break
        try: pair=bytes(gdb.selected_inferior().read_memory(fp,16))
        except gdb.error: break
        parent=int.from_bytes(pair[:8],'little'); pc=int.from_bytes(pair[8:],'little')
        result['frames'].append(hex(pc))
        if parent<=fp: break
        fp=parent
    return result
def reg(n): return int(gdb.parse_and_eval('$'+n)) & ((1 << 64) - 1)
def string(p):
    try:
        data=bytearray()
        for i in range(512):
            b=bytes(gdb.selected_inferior().read_memory(p+i,1))[0]
            if not b: break
            data.append(b)
        return data.decode('utf-8','replace')
    except gdb.error: return '<unreadable>'
class Entry(gdb.Breakpoint):
    def stop(self):
        n=reg('rdi'); a=[reg(r) for r in ['rsi','rdx','rcx','r8','r9']]
        frame=reg('rsp')+8
        a.append(int.from_bytes(bytes(gdb.selected_inferior().read_memory(frame+40,8)),'little'))
        row={'n':n,'args':[hex(x) for x in a], 'frame':hex(frame)}
        if n in [35,230] or (n==202 and a[3]==0 and a[1]&0x7f in [0,9]):
            try:
                row['user_context']=user_context(frame)
                if n in [35,230]:
                    ts=bytes(gdb.selected_inferior().read_memory(a[0] if n==35 else a[2],16))
                    row['request_timespec']=[int.from_bytes(ts[i:i+8],'little',signed=True) for i in [0,8]]
            except gdb.error: pass
        if n in [2,4,6,21,89]: row['path']=string(a[0])
        if n in [257,262,267]: row['path']=string(a[1])
        if n==1:
            # A valid lazy user page may not be readable by GDB until the
            # syscall actually accesses it. Retry on return, without stopping
            # the VM or changing guest paging merely to collect diagnostics.
            try: row['text']=bytes(gdb.selected_inferior().read_memory(a[1],min(a[2],512))).decode('utf-8','replace')
            except gdb.MemoryError: row['text']='<not resident at entry>'
        pending[frame]=row
        return False
class Leave(gdb.Breakpoint):
    def stop(self):
        row=pending.pop(reg('rsp'),None)
        if row:
            v=reg('rax'); row['ret']=v if v<2**63 else v-2**64
            if row.get('text')=='<not resident at entry>':
                try: row['text']=bytes(gdb.selected_inferior().read_memory(int(row['args'][1],16),min(int(row['args'][2],16),512))).decode('utf-8','replace')
                except gdb.MemoryError: pass
            print(json.dumps(row),flush=True)
        return False
Entry('*linux_syscall_handler',type=gdb.BP_HARDWARE_BREAKPOINT)
Leave('*linux_syscall_resume_frame',type=gdb.BP_HARDWARE_BREAKPOINT)
def show_pending():
    print(json.dumps({'inflight': list(pending.values())}), flush=True)
