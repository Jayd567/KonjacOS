/* Expected to terminate only this process when a demand page cannot be backed. */
#include <stdio.h>
#include <sys/mman.h>
int main(void) {
    unsigned long size=512UL*1024*1024;
    volatile unsigned char *p=mmap(0,size,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(p==MAP_FAILED) { puts("FAIL OOM reservation"); return 1; }
    for(unsigned long i=0;i<size;i+=4096) p[i]=1;
    puts("FAIL expected physical exhaustion on 256MiB guest");
    return 2;
}
