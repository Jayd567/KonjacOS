/* Hosted glibc regression: existing non-symlinks must return EINVAL. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static const char jvm[] = "/usr/lib/jvm/java-21-openjdk-amd64/lib/server/libjvm.so";
int main(void) {
    char buf[4096];
    const char *paths[] = {"/usr", jvm};
    for (unsigned i = 0; i < 2; ++i) {
        errno = 0;
        if (readlink(paths[i], buf, sizeof buf) != -1 || errno != EINVAL) {
            printf("FAIL readlink %s errno=%d\n", paths[i], errno); return 1;
        }
        errno = 0;
        if (readlinkat(AT_FDCWD, paths[i], buf, sizeof buf) != -1 || errno != EINVAL) {
            printf("FAIL readlinkat %s errno=%d\n", paths[i], errno); return 2;
        }
    }
    if (readlink("/usr/no-such-konjac-file", buf, sizeof buf) != -1 || errno != ENOENT) {
        puts("FAIL missing path"); return 3;
    }
    if (readlink("/proc/self/exe", buf, sizeof buf) <= 0) {
        puts("FAIL proc executable link"); return 4;
    }
    if (!realpath(jvm, buf) || strcmp(buf, jvm)) {
        printf("FAIL realpath errno=%d\n", errno); return 5;
    }
    puts("PASS readlink errors and glibc realpath");
    return 0;
}
