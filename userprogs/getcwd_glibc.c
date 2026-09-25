#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
#define CHECK(x,s) do { if(!(x)) { printf("FAIL " s " errno=%d\n",errno); return 1; } } while(0)
int main(void) {
    const char *expected=getenv("EXPECT_CWD");
    if(expected) {
        char path[4096];
        CHECK(getcwd(path,sizeof path)==path && !strcmp(path,expected),"shell directory reflected by getcwd");
        puts("PASS getcwd reflects shell directory"); return 0;
    }
    char guard[4]={'a','b','c','d'};
    CHECK(syscall(SYS_getcwd,guard+1,2)==2,"raw getcwd length");
    CHECK(guard[0]=='a' && guard[1]=='/' && guard[2]==0 && guard[3]=='d',"path/terminator/bounds");
    guard[1]='b';
    CHECK(syscall(SYS_getcwd,guard+1,1)==-1 && errno==ERANGE && guard[1]=='b',"short buffer");
    CHECK(syscall(SYS_getcwd,guard,0)==-1 && errno==ERANGE,"zero size");
    CHECK(syscall(SYS_getcwd,0,4096)==-1 && errno==EFAULT,"NULL raw output");
    CHECK(syscall(SYS_getcwd,(void*)-1,4096)==-1 && errno==EFAULT,"overflow raw output");
    char *p=mmap(0,4096,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    CHECK(p!=MAP_FAILED && getcwd(p,4096)==p && !strcmp(p,"/"),"glibc lazy destination");
    munmap(p,4096);
    p=getcwd(0,0);
    CHECK(p && !strcmp(p,"/"),"glibc allocating getcwd");
    free(p);
    puts("PASS getcwd raw length, bounds, ERANGE, lazy and allocating glibc wrappers");
    return 0;
}
