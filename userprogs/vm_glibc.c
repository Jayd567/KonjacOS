/* Hosted guest tests for reservation lifetime and permissions. */
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

static volatile unsigned char *guard;
static volatile sig_atomic_t faults;
static volatile sig_atomic_t restore_mapping;
static void *held[2100];
static void on_fault(int sig) {
    (void)sig;
    ++faults;
    if(restore_mapping) mmap((void*)guard,4096,PROT_READ|PROT_WRITE,
                            MAP_FIXED|MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    else if(mprotect((void*)guard,4096,PROT_READ|PROT_WRITE)) _exit(90);
}
static unsigned char expected(unsigned long i) { return (unsigned char)(i*37+i/251); }
static void *child(void *arg) {
    volatile unsigned char *p=arg;
    p[8192]=73; /* Parent reservation was never touched before clone. */
    unsigned char *q=mmap(0,4096,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(q==MAP_FAILED) return (void*)1;
    q[0]=91;
    return q; /* Parent must see a child-created reservation too. */
}
#define REQUIRE(x,msg) do { if(!(x)) { puts("FAIL " msg); return 1; } } while(0)
int main(void) {
    int fd=open("/IOPAT.BIN",O_RDONLY);
    REQUIRE(fd>=0,"vm fixture open");
    REQUIRE(mmap(0,4096,1,MAP_PRIVATE,-1,0)==MAP_FAILED && errno==EBADF,"invalid mmap fd");
    REQUIRE(mmap(0,4096,1,MAP_PRIVATE,fd,1)==MAP_FAILED && errno==EINVAL,"unaligned offset");
    REQUIRE(mmap((void*)0x7100000001UL,4096,3,MAP_FIXED|MAP_PRIVATE|MAP_ANONYMOUS,-1,0)==MAP_FAILED && errno==EINVAL,"unaligned fixed address");
    REQUIRE(mmap(0,SIZE_MAX,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0)==MAP_FAILED,"overflow length");
    unsigned char *shared=mmap(0,4096,PROT_READ,MAP_SHARED,fd,0);
    REQUIRE(shared!=MAP_FAILED,"read-only shared map");
    REQUIRE(mprotect(shared,4096,PROT_READ|PROT_WRITE)==-1,"shared write upgrade rejected");
    munmap(shared,4096);
    unsigned char *p=mmap(0,12288,1,MAP_PRIVATE,fd,4096);
    REQUIRE(p!=MAP_FAILED,"file reservation");
    REQUIRE(mmap(p+4096,4096,3,MAP_FIXED|MAP_PRIVATE|MAP_ANONYMOUS,-1,0)==p+4096,"middle replacement");
    close(fd);
    REQUIRE(p[509]==expected(4605) && p[8192+7]==expected(12295) && p[4096]==0,"split backing offsets");
    munmap(p,12288);
    struct sigaction action={0};
    action.sa_handler=on_fault;
    sigemptyset(&action.sa_mask);
    REQUIRE(sigaction(SIGSEGV,&action,0)==0,"install fault handler");
    guard=mmap(0,4096,PROT_NONE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    REQUIRE(guard!=MAP_FAILED,"guard reservation");
    *guard=1;
    REQUIRE(faults==1 && *guard==1,"untouched PROT_NONE enforcement");
    munmap((void*)guard,4096);
    guard=mmap(0,4096,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    REQUIRE(mprotect((void*)guard,4096,1)==0,"lazy mprotect");
    *guard=2;
    REQUIRE(faults==2,"lazy read-only enforcement");
    munmap((void*)guard,4096);
    p=mmap(0,12288,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    guard=p+4096;
    REQUIRE(munmap((void*)guard,4096)==0,"partial munmap");
    restore_mapping=1;
    *guard=3;
    REQUIRE(faults==3,"unmapped hole stays unmapped");
    munmap(p,12288);
    p=mmap(0,12288,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    pthread_t thread;
    void *q=0;
    REQUIRE(pthread_create(&thread,0,child,p)==0 && pthread_join(thread,&q)==0,"shared vm thread join");
    REQUIRE(q && q!=(void*)1 && p[8192]==73 && *(unsigned char*)q==91,"shared vm ownership");
    munmap(q,4096);
    munmap(p,12288);
    p=mmap(0,12288,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    REQUIRE(p!=MAP_FAILED,"capacity sentinel reservation");
    p[4096]=42;
    unsigned count=0;
    for(;count<2100;++count) {
        held[count]=mmap(0,4096,PROT_NONE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
        if(held[count]==MAP_FAILED) break;
    }
    REQUIRE(count>0 && count<2100 && errno==ENOMEM,"bounded metadata exhaustion");
    REQUIRE(mprotect(p+4096,4096,1)==-1 && errno==ENOMEM,"protect failure rollback");
    REQUIRE(munmap(p+4096,4096)==-1 && errno==ENOMEM,"unmap failure rollback");
    REQUIRE(mmap(p+4096,4096,3,MAP_FIXED|MAP_PRIVATE|MAP_ANONYMOUS,-1,0)==MAP_FAILED && errno==ENOMEM,
            "fixed replacement failure rollback");
    REQUIRE(p[4096]==42,"failed edits preserve original data");
    p[4096]=43;
    for(unsigned i=0;i<count;++i) REQUIRE(munmap(held[i],4096)==0,"metadata slot release");
    REQUIRE(mprotect(p+4096,4096,1)==0 && munmap(p,12288)==0,"metadata capacity recovered");
    puts("PASS mmap validation, splitting, lazy permissions, holes, shared threads, capacity rollback");
    return 0;
}
