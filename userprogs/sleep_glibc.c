#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#define CHECK(x,s) do { if(!(x)) { printf("FAIL " s " errno=%d\n",errno); return 1; } } while(0)
static atomic_int ready,done,progress;
static int worker_error;
static long long now(int clock) { struct timespec t; clock_gettime(clock,&t); return (long long)t.tv_sec*1000000000+t.tv_nsec; }
static void *worker(void *unused) {
    atomic_store(&ready,1);
    while(!atomic_load(&done)) {
        atomic_fetch_add(&progress,1);
        if(syscall(SYS_futex,0,FUTEX_WAKE,100,0,0,0)!=0) worker_error=1;
        sched_yield();
    }
    return unused;
}
int main(void) {
    CHECK(sched_yield()==0,"sched_yield");
    struct timespec t={0,30000000},rem={123,456};
    long long start=now(CLOCK_MONOTONIC);
    CHECK(syscall(SYS_clock_nanosleep,CLOCK_REALTIME,0,&t,&rem)==0,"raw clock sleep");
    CHECK(now(CLOCK_MONOTONIC)-start>=30000000,"relative sleep too early");
    CHECK(rem.tv_sec==123 && rem.tv_nsec==456,"successful sleep touched remainder");
    CHECK(syscall(SYS_nanosleep,&t,0)==0 && nanosleep(&t,0)==0,"raw and glibc nanosleep");
    int clocks[]={CLOCK_REALTIME,CLOCK_MONOTONIC,CLOCK_BOOTTIME};
    for(unsigned i=0;i<3;i++) {
        long long end=now(clocks[i])+30000000;
        t=(struct timespec){end/1000000000,end%1000000000};
        CHECK(clock_nanosleep(clocks[i],TIMER_ABSTIME,&t,(void*)1)==0 && now(clocks[i])>=end,"absolute sleep");
        t=(struct timespec){0,0};
        CHECK(clock_nanosleep(clocks[i],TIMER_ABSTIME,&t,0)==0,"past deadline");
    }
    CHECK(nanosleep(&t,0)==0,"zero sleep");
    t.tv_nsec=1; start=now(CLOCK_MONOTONIC);
    CHECK(nanosleep(&t,0)==0 && now(CLOCK_MONOTONIC)>start,"sub-tick sleep");
    struct timespec invalid[]={{-1,0},{0,-1},{0,1000000000}};
    for(unsigned i=0;i<3;i++) CHECK(clock_nanosleep(CLOCK_MONOTONIC,0,&invalid[i],0)==EINVAL,"invalid timespec");
    CHECK(clock_nanosleep(CLOCK_THREAD_CPUTIME_ID,0,&t,0)==EINVAL,"unsupported clock");
    CHECK(syscall(SYS_nanosleep,0,0)==-1 && errno==EFAULT,"NULL request");
    unsigned char bytes[sizeof t+1]; t=(struct timespec){0,1}; memcpy(bytes+1,&t,sizeof t);
    CHECK(syscall(SYS_nanosleep,bytes+1,0)==0,"unaligned request");
    void *lazy=mmap(0,4096,3,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    CHECK(lazy!=MAP_FAILED && syscall(SYS_nanosleep,lazy,0)==0,"untouched zero request"); munmap(lazy,4096);
    pthread_t thread;
    CHECK(!pthread_create(&thread,0,worker,0),"worker create");
    while(!atomic_load(&ready)) sched_yield();
    atomic_store(&progress,0);
    t=(struct timespec){0,200000000}; start=now(CLOCK_MONOTONIC);
    CHECK(nanosleep(&t,0)==0 && now(CLOCK_MONOTONIC)-start>=200000000,"sleep immune to futex wake");
    atomic_store(&done,1);
    CHECK(!pthread_join(thread,0) && !worker_error && atomic_load(&progress)>0,"sleep permits sibling progress");
    puts("PASS sleep deadlines/clocks, validation, lazy requests, sibling progress and futex isolation");
    return 0;
}
