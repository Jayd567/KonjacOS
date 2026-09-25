#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdatomic.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#define CHECK(x,s) do { if(!(x)) { printf("FAIL " s " errno=%d\n",errno); return 1; } } while(0)
static unsigned word=7;
static atomic_int ready;
static long result;
static int wait_op=FUTEX_WAIT_BITSET|FUTEX_PRIVATE_FLAG;
static struct timespec now(int clock) { struct timespec t; clock_gettime(clock,&t); return t; }
static long long ns(struct timespec t) { return (long long)t.tv_sec*1000000000+t.tv_nsec; }
static struct timespec future(int clock,long n) { struct timespec t=now(clock); t.tv_nsec+=n; t.tv_sec+=t.tv_nsec/1000000000; t.tv_nsec%=1000000000; return t; }
static void *waiter(void *arg) {
    struct timespec t=*(struct timespec*)arg;
    // Start the finite deadline after pthread startup, not before creation.
    if(t.tv_sec!=INT64_MAX) t=future(CLOCK_MONOTONIC,2000000000);
    atomic_store(&ready,1);
    result=syscall(SYS_futex,&word,wait_op,7,&t,0,FUTEX_BITSET_MATCH_ANY);
    return 0;
}
int main(void) {
    struct timespec t={0,30000000},start=now(CLOCK_MONOTONIC);
    CHECK(syscall(SYS_futex,&word,FUTEX_WAIT|FUTEX_PRIVATE_FLAG,7,&t,0,0)==-1 && errno==ETIMEDOUT,"relative timeout");
    CHECK(ns(now(CLOCK_MONOTONIC))-ns(start)>=30000000,"relative expiry too early");
    t=(struct timespec){0,0};
    CHECK(syscall(SYS_futex,&word,FUTEX_WAIT,7,&t,0,0)==-1 && errno==ETIMEDOUT,"zero relative timeout");
    t.tv_nsec=1; start=now(CLOCK_MONOTONIC);
    CHECK(syscall(SYS_futex,&word,FUTEX_WAIT,7,&t,0,0)==-1 && errno==ETIMEDOUT,"sub-tick timeout");
    CHECK(ns(now(CLOCK_MONOTONIC))>ns(start),"sub-tick expiry too early");
    CHECK(syscall(SYS_futex,&word,FUTEX_WAKE,1,0,0,0)==0,"expired waiter cleanup");
    CHECK(syscall(SYS_futex,&word,FUTEX_WAIT_BITSET,7,&t,0,0)==-1 && errno==EINVAL,"zero mask");
    CHECK(syscall(SYS_futex,&word,FUTEX_WAIT_BITSET,7,&t,0,1)==-1 && errno==ENOSYS,"unsupported selective mask");
    for(int clock=0;clock<2;clock++) {
        int op=FUTEX_WAIT_BITSET|FUTEX_PRIVATE_FLAG|(clock==CLOCK_REALTIME?FUTEX_CLOCK_REALTIME:0);
        t=future(clock,30000000);
        CHECK(syscall(SYS_futex,&word,op,7,&t,0,FUTEX_BITSET_MATCH_ANY)==-1 && errno==ETIMEDOUT,"absolute timeout");
        CHECK(ns(now(clock))>=ns(t),"absolute expiry too early");
        t=(struct timespec){0,0};
        CHECK(syscall(SYS_futex,&word,op,8,&t,0,FUTEX_BITSET_MATCH_ANY)==-1 && errno==EAGAIN,"mismatch precedes expiry");
        CHECK(syscall(SYS_futex,&word,op,7,&t,0,FUTEX_BITSET_MATCH_ANY)==-1 && errno==ETIMEDOUT,"expired deadline");
    }
    struct timespec invalid[]={{-1,0},{0,-1},{0,1000000000}};
    for(unsigned i=0;i<3;i++) CHECK(syscall(SYS_futex,&word,FUTEX_WAIT,7,&invalid[i],0,0)==-1 && errno==EINVAL,"invalid timespec");
    for(int i=0;i<6;i++) {
        pthread_t thread; atomic_store(&ready,0); result=-99;
        t=i<4?(struct timespec){0,0}:(struct timespec){INT64_MAX,999999999};
        wait_op=(i==5?FUTEX_WAIT:FUTEX_WAIT_BITSET)|FUTEX_PRIVATE_FLAG;
        CHECK(!pthread_create(&thread,0,waiter,&t),"create waiter");
        while(!atomic_load(&ready)) syscall(SYS_sched_yield);
        struct timespec watchdog=future(CLOCK_MONOTONIC,2000000000);
        long woke=0;
        while(ns(now(CLOCK_MONOTONIC))<ns(watchdog)) {
            woke=syscall(SYS_futex,&word,FUTEX_WAKE|FUTEX_PRIVATE_FLAG,1,0,0,0);
            if(woke) break;
            syscall(SYS_sched_yield);
        }
        CHECK(woke==1 && !pthread_join(thread,0) && result==0,"wake before timeout");
        CHECK(syscall(SYS_futex,&word,FUTEX_WAKE,1,0,0,0)==0,"stale waiter");
    }
    pthread_mutex_t mutex=PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t cond=PTHREAD_COND_INITIALIZER;
    CHECK(!pthread_mutex_lock(&mutex),"mutex lock");
    t=future(CLOCK_REALTIME,30000000);
    CHECK(pthread_cond_timedwait(&cond,&mutex,&t)==ETIMEDOUT,"glibc condition timeout");
    CHECK(!pthread_mutex_unlock(&mutex),"mutex unlock");
    puts("PASS timed futex relative/absolute clocks, validation, wake cleanup, glibc condition timeout");
    return 0;
}
