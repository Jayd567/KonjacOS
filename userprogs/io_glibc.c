/* Hosted glibc probe of the existing buffered-file and mmap paths. */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>
static unsigned char expected(unsigned long i) { return (unsigned char)(i * 37 + i / 251); }
static int check(const unsigned char *p, unsigned long at, unsigned long n) {
    for (unsigned long i=0; i<n; ++i) if (p[i] != expected(at+i)) return 0;
    return 1;
}
int main(void) {
    unsigned char buf[8192];
    int fd=open("/IOPAT.BIN", O_RDONLY);
    if (fd<0) { puts("FAIL open fixture"); return 1; }
    if (read(fd,buf,17)!=17 || !check(buf,0,17)) { puts("FAIL initial read"); return 2; }
    unsigned long offsets[]={509,8190,32766,65530};
    for (unsigned i=0;i<4;++i) {
        unsigned long want=offsets[i]+4096>65539 ? 65539-offsets[i] : 4096;
        ssize_t n=pread(fd,buf,4096,offsets[i]);
        if (n!=(ssize_t)want || !check(buf,offsets[i],want)) { puts("FAIL sector/cluster pread"); return 3; }
    }
    if (read(fd,buf,17)!=17 || !check(buf,17,17)) { puts("FAIL pread changed position"); return 4; }
    errno=0;
    off_t seek=lseek(fd,509,SEEK_SET);
    if (seek!=509) { printf("FAIL lseek SET errno=%d\n",errno); return 17; }
    if (lseek(fd,-9,SEEK_CUR)!=500 || lseek(fd,-9,SEEK_END)!=65530 ||
        read(fd,buf,4096)!=9 || !check(buf,65530,9)) { puts("FAIL seek CUR/END"); return 18; }
    if (lseek(fd,65540,SEEK_SET)!=65540 || read(fd,buf,1)!=0) { puts("FAIL seek past EOF"); return 19; }
    if (lseek(fd,-1,SEEK_SET)!=-1 || errno!=EINVAL ||
        lseek(fd,INT64_MAX,SEEK_CUR)!=-1 || errno!=EINVAL ||
        lseek(fd,0,123)!=-1 || errno!=EINVAL || lseek(fd,0,SEEK_CUR)!=65540) {
        puts("FAIL invalid seek changed position"); return 20;
    }
    if (lseek(-1,0,SEEK_SET)!=-1 || errno!=EBADF || lseek(fd,509,SEEK_SET)!=509) {
        puts("FAIL seek fd/restore"); return 21;
    }
    unsigned char *p=mmap(0,8195,PROT_READ,MAP_PRIVATE,fd,4096);
    if (p==MAP_FAILED || !check(p,4096,8195)) { puts("FAIL offset file mmap"); return 5; }
    munmap(p,8195);
    p=mmap(0,8192,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(p==MAP_FAILED) { puts("FAIL anonymous mmap"); return 6; }
    for(unsigned i=0;i<8192;++i) {
        if(p[i]!=0) { puts("FAIL anonymous zero fill"); return 7; }
        p[i]=(unsigned char)i;
    }
    for(unsigned i=0;i<8192;++i) if(p[i]!=(unsigned char)i) { puts("FAIL anonymous write"); return 8; }
    munmap(p,8192);
    // Do not touch these pages before the syscall: copying to a lazy user
    // buffer must not fault while holding the scheduler's task-table lock.
    p=mmap(0,8192,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if (p==MAP_FAILED) { puts("FAIL lazy buffer mmap"); return 9; }
    if (pread(fd,p,4096,509)!=4096 || !check(p,509,4096)) {
        puts("FAIL pread into untouched page"); return 10;
    }
    // Nested #PF during a syscall must preserve the user's saved SIMD state.
    // The syscall's destination is a fresh page and is not touched beforehand.
    unsigned char *fresh=mmap(0,4096,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(fresh==MAP_FAILED) { puts("FAIL SIMD test mapping"); return 23; }
    uint64_t lanes[2]={0x0123456789abcdefULL,0xfedcba9876543210ULL}, got[2];
    register unsigned long off __asm__("r10")=509;
    long fp_n;
    __asm__ volatile("movdqu %2,%%xmm15; syscall; movdqu %%xmm15,%1"
        : "=a"(fp_n), "=m"(got) : "m"(lanes), "a"(17UL),
          "D"((unsigned long)fd), "S"(fresh), "d"(17UL), "r"(off)
        : "rcx", "r11", "xmm15", "memory", "cc");
    if(fp_n!=17 || got[0]!=lanes[0] || got[1]!=lanes[1] || !check(fresh,509,17)) {
        puts("FAIL nested page fault SIMD preservation"); return 22;
    }
    munmap(fresh,4096);
    if (read(fd,p+4096,4096)!=4096 || !check(p+4096,seek==-1 ? 34 : 509,4096)) {
        puts("FAIL read into untouched page"); return 11;
    }
    munmap(p,8192);
    p=mmap(0,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if (p==MAP_FAILED) { puts("FAIL native buffer mmap"); return 12; }
    long native_n;
    __asm__ volatile("int $0x80" : "=a"(native_n) : "a"(3UL), "D"((unsigned long)fd), "S"(p), "d"(17UL) : "memory", "cc");
    if (native_n!=17 || !check(p,(seek==-1 ? 34 : 509)+4096,17)) {
        puts("FAIL native read into untouched page"); return 13;
    }
    munmap(p,4096);
    if (pread(fd,buf,1,65539)!=0 || pread(fd,buf,1,65540)!=0 || pread(fd,0,0,99999)!=0) {
        puts("FAIL pread EOF"); return 14;
    }
    if (pread(fd,buf,1,-1)!=-1 || errno!=EINVAL) { puts("FAIL negative offset"); return 15; }
    ssize_t n;
    do { n=read(fd,buf,sizeof buf); } while(n>0);
    if(n!=0 || read(fd,buf,1)!=0) { puts("FAIL read EOF"); return 16; }
    close(fd);
    puts("PASS seek, cross-sector pread, lazy buffers, file mmap"); return 0;
}
