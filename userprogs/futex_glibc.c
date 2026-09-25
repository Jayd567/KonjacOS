/* Hosted Linux/glibc regression fixture, not part of the freestanding kernel. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <errno.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <linux/futex.h>
static unsigned word = 7;
static void *worker(void *arg) {
    for (volatile unsigned i = 0; i < 100000000; ++i) {}
    return arg;
}
int main(void) {
    unsigned ops[] = {FUTEX_WAIT_BITSET, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
                      FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME};
    for (unsigned i = 0; i < 3; ++i) {
        errno = 0;
        long r = syscall(SYS_futex, &word, ops[i], 8, 0, 0, FUTEX_BITSET_MATCH_ANY);
        if (r != -1 || errno != EAGAIN) {
            printf("FAIL wait-bitset mismatch: errno=%d\n", errno); return 1;
        }
    }
    if (syscall(SYS_futex, &word, FUTEX_WAIT_BITSET, 8, 0, 0, 0) != -1 || errno != EINVAL) {
        puts("FAIL zero mask"); return 2;
    }
    pthread_t t;
    void *result = 0;
    if (pthread_create(&t, 0, worker, (void *)(uintptr_t)42) != 0) {
        puts("FAIL pthread_create"); return 3;
    }
    if (pthread_join(t, &result) != 0 || result != (void *)(uintptr_t)42) {
        puts("FAIL pthread_join"); return 4;
    }
    puts("PASS futex-bitset and glibc pthread_join");
    return 0;
}
