/* Regular-file reads must span staging chunks without allocating file-sized buffers. */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>
#define CHECK(x,s) do { if(!(x)) { printf("FAIL " s " errno=%d\n",errno); return 1; } } while(0)
static int pattern(unsigned char *p,unsigned long offset,unsigned long n) {
    for(unsigned long i=0;i<n;i++) if(p[i]!=(unsigned char)((offset+i)*37+(offset+i)/251)) return 0;
    return 1;
}
int main(void) {
    unsigned char *p=mmap(0,73728,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    CHECK(p!=MAP_FAILED,"lazy buffer");
    int fd=open("/IOPAT.BIN",O_RDONLY);
    CHECK(fd>=0,"pattern open");
    CHECK(pread(fd,p+3,49154,509)==49154 && pattern(p+3,509,49154),"large unaligned lazy pread");
    CHECK(lseek(fd,0,SEEK_CUR)==0,"pread position");
    CHECK(read(fd,p,70000)==65539 && pattern(p,0,65539),"large read through EOF");
    CHECK(lseek(fd,0,SEEK_CUR)==65539 && read(fd,p,1)==0,"read position and EOF");
    CHECK(pread(fd,p,70000,60000)==5539 && pattern(p,60000,5539),"pread partial EOF");
    CHECK(pread(fd,0,0,0)==0,"zero read");
    CHECK(pread(-1,p,70000,0)==-1 && errno==EBADF,"invalid fd");
    close(fd);
    fd=open("/usr/lib/jvm/java-21-openjdk-amd64/lib/modules",O_RDONLY);
    CHECK(fd>=0,"JDK archive open");
    CHECK(pread(fd,p,49154,0x223cd9)==49154,"complete String class read");
    uint32_t hash=2166136261u;
    for(unsigned i=0;i<49154;i++) hash=(hash^p[i])*16777619u;
    CHECK(hash==0x4f7337c4u,"String class bytes match verified archive");
    close(fd); munmap(p,73728);
    puts("PASS multi-chunk read/pread, lazy destination, EOF, offsets and real String class bytes");
    return 0;
}
