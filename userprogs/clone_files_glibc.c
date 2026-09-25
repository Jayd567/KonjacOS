#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>
#define CHECK(x,s) do { if(!(x)) { printf("FAIL " s " errno=%d\n",errno); return 1; } } while(0)
static int fd,child_fd,worker_error;
static atomic_int release_child;
static pthread_t survivor;
static int pattern(unsigned char *p,unsigned long offset,unsigned long n) {
    for(unsigned long i=0;i<n;i++) if(p[i]!=(unsigned char)((offset+i)*37+(offset+i)/251)) return 0;
    return 1;
}
static void *reader(void *arg) {
    unsigned char *p=mmap(0,8192,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(p==MAP_FAILED || read(fd,p,8192)!=8192 || !pattern(p,17,8192)) worker_error=1;
    if(p!=MAP_FAILED) munmap(p,8192);
    return arg;
}
static void *closer(void *arg) {
    if(close(fd)) worker_error=2;
    child_fd=open("/IOPAT.BIN",O_RDONLY);
    if(child_fd<0) worker_error=3;
    return arg;
}
static void *after_creator(void *arg) {
    while(!atomic_load(&release_child)) sched_yield();
    unsigned char p[17];
    if(pread(child_fd,p,sizeof p,509)!=sizeof p || !pattern(p,509,sizeof p)) worker_error=4;
    return arg;
}
static void *creator(void *arg) {
    child_fd=open("/IOPAT.BIN",O_RDONLY);
    if(child_fd<0 || pthread_create(&survivor,0,after_creator,0)) worker_error=5;
    return arg;
}
int main(void) {
    unsigned char p[17]; pthread_t t;
    fd=open("/IOPAT.BIN",O_RDONLY);
    CHECK(fd>=0 && read(fd,p,17)==17,"parent open/read");
    CHECK(!pthread_create(&t,0,reader,0) && !pthread_join(t,0) && !worker_error,"child inherited descriptor/lazy read");
    CHECK(lseek(fd,0,SEEK_CUR)==8209,"shared sequential position");
    CHECK(!pthread_create(&t,0,closer,0) && !pthread_join(t,0) && !worker_error,"child close/open");
    CHECK(child_fd==fd && read(child_fd,p,17)==17 && pattern(p,0,17),"shared descriptor replacement");
    CHECK(!close(child_fd),"parent close");
    CHECK(!pthread_create(&t,0,creator,0) && !pthread_join(t,0) && !worker_error,"nested creator");
    atomic_store(&release_child,1);
    CHECK(!pthread_join(survivor,0) && !worker_error,"descriptor survives creator exit");
    CHECK(!close(child_fd),"survivor descriptor cleanup");
    puts("PASS CLONE_FILES inheritance, shared offsets/open/close, lazy reads and creator lifetime");
    return 0;
}
