/* Hosted guest probe: real JDK archive larger than the kernel heap. */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysinfo.h>
#include <unistd.h>
#include <string.h>

int main(void) {
    struct sysinfo baseline, touched, released;
    if(sysinfo(&baseline) || baseline.mem_unit!=1 || !baseline.freeram) {
        puts("FAIL memory baseline/units"); return 7;
    }
    for (int run=0; run<4; ++run) {
        int fd=open("/usr/lib/jvm/java-21-openjdk-amd64/lib/modules",O_RDONLY);
        struct stat st;
        if(fd<0 || fstat(fd,&st) || st.st_size<=96*1024*1024) {
            puts("FAIL large archive open/stat"); return 1;
        }
        uint32_t magic=0;
        unsigned char tail[64];
        if(pread(fd,&magic,4,0)!=4 || magic!=0xcafedada ||
           pread(fd,tail,64,st.st_size-64)!=64 || pread(fd,tail,1,0x100000000ULL)!=0) {
            puts("FAIL archive header/tail read"); return 2;
        }
        unsigned char *p=mmap(0,st.st_size,PROT_READ,MAP_PRIVATE,fd,0);
        if(p==MAP_FAILED) { puts("FAIL large archive mmap"); return 3; }
        close(fd);
        if(memcmp(p,&magic,4) || memcmp(p+st.st_size-64,tail,64)) {
            puts("FAIL archive mapping after close"); return 4;
        }
        for(long i=st.st_size;i<((st.st_size+4095)&~4095L);++i)
            if(p[i]) { puts("FAIL archive EOF padding"); return 5; }
        if(sysinfo(&touched) || touched.freeram*touched.mem_unit+1024*1024 < baseline.freeram*baseline.mem_unit) {
            puts("FAIL mapping allocated archive-sized physical backing"); return 8;
        }
        if(munmap(p,st.st_size)) { puts("FAIL archive munmap"); return 6; }
        if(sysinfo(&released) || released.freeram*released.mem_unit+128*1024 < baseline.freeram*baseline.mem_unit) {
            puts("FAIL repeated map physical frame growth"); return 9;
        }
    }
    puts("PASS large archive reads, lazy mmap after close, EOF padding, bounded frames, repeat");
    return 0;
}
