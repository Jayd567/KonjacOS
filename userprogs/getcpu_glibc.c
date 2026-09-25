/* Guest probe of Linux getcpu and glibc's fallback without a vDSO/rseq. */
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define CHECK(x,msg) do { if(!(x)) { printf("FAIL " msg " errno=%d\n",errno); return 1; } } while(0)
int main(void) {
    struct { uint32_t before,cpu,middle,node,after; } out={11,UINT32_MAX,22,UINT32_MAX,33};
    CHECK(syscall(SYS_getcpu,&out.cpu,&out.node,0)==0,"getcpu syscall");
    CHECK(out.cpu==0 && out.node==0 && out.before==11 && out.middle==22 && out.after==33,"getcpu value/width");
    out.cpu=out.node=99;
    CHECK(syscall(SYS_getcpu,0,&out.node,0)==0 && out.node==0 && out.cpu==99,"NULL cpu");
    CHECK(syscall(SYS_getcpu,&out.cpu,0,0)==0 && out.cpu==0,"NULL node");
    CHECK(syscall(SYS_getcpu,0,0,(void*)1)==0,"NULL outputs and ignored cache");
    unsigned char bytes[10];
    memset(bytes,0xa5,sizeof bytes);
    CHECK(syscall(SYS_getcpu,bytes+1,bytes+5,0)==0,"unaligned outputs");
    CHECK(bytes[0]==0xa5 && bytes[9]==0xa5,"unaligned guard bytes");
    for(unsigned i=1;i<9;++i) CHECK(bytes[i]==0,"unaligned output contents");
    void *lazy=mmap(0,8192,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    CHECK(lazy!=MAP_FAILED,"lazy output allocation");
    CHECK(syscall(SYS_getcpu,lazy,(char*)lazy+4096,0)==0 && *(uint32_t*)lazy==0 && *(uint32_t*)((char*)lazy+4096)==0,"lazy output write");
    CHECK(munmap(lazy,8192)==0,"lazy output cleanup");
    CHECK(sched_getcpu()==0,"glibc sched_getcpu");
    puts("PASS getcpu values, widths, NULL/unaligned/lazy outputs, glibc sched_getcpu");
    return 0;
}
