/* Prints statfs/fstatfs results for a path, to check against mtools. */
#include <fcntl.h>
#include <stdio.h>
#include <sys/vfs.h>
#include <unistd.h>

static void show(const char *label, int rc, const struct statfs *s) {
    if (rc != 0) { perror(label); return; }
    printf("%s type=%#lx bsize=%ld blocks=%lu bfree=%lu bavail=%lu namelen=%ld frsize=%ld flags=%#lx\n",
           label, (unsigned long)s->f_type, (long)s->f_bsize, (unsigned long)s->f_blocks,
           (unsigned long)s->f_bfree, (unsigned long)s->f_bavail, (long)s->f_namelen,
           (long)s->f_frsize, (unsigned long)s->f_flags);
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/";
    struct statfs s;
    show("statfs", statfs(path, &s), &s);
    int fd = open(path, O_RDONLY);
    show("fstatfs", fstatfs(fd, &s), &s);
    show("missing", statfs("/no/such/file", &s), &s);
    return 0;
}
