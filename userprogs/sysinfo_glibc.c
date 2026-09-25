/* Hosted guest check of the x86_64 Linux sysinfo layout and memory units. */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/sysinfo.h>

_Static_assert(sizeof(struct sysinfo)==112,"x86_64 sysinfo size");
_Static_assert(offsetof(struct sysinfo,mem_unit)==104,"x86_64 mem_unit offset");
int main(void) {
    struct { uint64_t before; struct sysinfo info; uint64_t after; } out;
    memset(&out,0xa5,sizeof out);
    if(sysinfo(&out.info) || out.before!=UINT64_C(0xa5a5a5a5a5a5a5a5) ||
       out.after!=UINT64_C(0xa5a5a5a5a5a5a5a5)) {
        puts("FAIL sysinfo result/size"); return 1;
    }
    if(out.info.mem_unit!=1 || out.info.freehigh || out.info.totalhigh) {
        printf("FAIL sysinfo memory units: unit=%u freehigh=%lu\n",out.info.mem_unit,out.info.freehigh); return 2;
    }
    if(out.info.totalram<128UL*1024*1024 || !out.info.freeram ||
       out.info.freeram>out.info.totalram || out.info.totalswap || out.info.freeswap) {
        puts("FAIL sysinfo memory values"); return 3;
    }
    puts("PASS sysinfo ABI, byte units, memory values and bounds"); return 0;
}
