/* Proves printf and file I/O work together, both parts of the DOOM-
 * porting groundwork's libc shim (see printf.c and ../src/cfile.rs).
 *
 * No standard headers -- see cdemo.c/printf.c for why. Just the
 * declarations this file needs.
 */

typedef unsigned long size_t;
typedef struct FILE FILE;

extern int printf(const char *fmt, ...);
extern int fprintf(FILE *file, const char *fmt, ...);
extern FILE *fopen(const char *path, const char *mode);
extern int fclose(FILE *file);
extern size_t fread(void *ptr, size_t size, size_t nmemb, FILE *file);

/* Runs the whole demo, returns 0 on success or a small negative code
 * identifying which step failed -- `cmd_cio` (commands.rs) reports
 * whichever comes back. */
int iodemo_run(void) {
    printf("iodemo: printf itself works -- %d %s %x (expect '42 hello 2a')\n", 42, "hello", 0x2a);

    FILE *out = fopen("CIODEMO.TXT", "w");
    if (out == 0) {
        return -1;
    }
    fprintf(out, "written by C: %d,%d,%d,%d\n", 1, 2, 3, 4);
    if (fclose(out) != 0) {
        return -2;
    }

    FILE *in = fopen("CIODEMO.TXT", "r");
    if (in == 0) {
        return -3;
    }
    char buf[128];
    size_t n = fread(buf, 1, sizeof(buf) - 1, in);
    buf[n] = '\0';
    fclose(in);

    printf("iodemo: read back from disk: %s", buf);
    return 0;
}
