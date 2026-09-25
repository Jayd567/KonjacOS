/* A small, self-contained printf family for freestanding C code compiled
 * into KonjacOS (see build.rs). No libc, no <stdio.h>/<stdarg.h> -- this
 * *is* the implementation those headers would normally declare, hand-
 * written against the compiler's own variadic-argument builtins
 * (__builtin_va_*), which GCC and Clang both provide even in freestanding
 * mode without needing any header at all.
 *
 * Supports the specifiers real-world C code actually reaches for: %d/%i,
 * %u, %x/%X, %o, %c, %s, %p, %%, the 'l'/'ll' length modifiers (so %ld and
 * %lu work), zero-padding and a minimum field width (%08x, %4d), and a
 * precision on %s (%.8s). What it deliberately does NOT support: floating
 * point (%f/%e/%g -- doomgeneric doesn't need them for anything on the
 * critical path; add them later if that changes), the '+'/' '/'#' flags,
 * left-justification ('-'), or %n. Good enough to be a real, useful
 * printf -- not a claim of full C99 conformance.
 *
 * `printf`/`vprintf` format into a fixed-size stack buffer and then hand
 * the result to `konjac_write` (implemented in Rust, see cfile.rs) to
 * actually reach the screen; output longer than that buffer is truncated
 * rather than corrupting anything. `fprintf` goes through `fwrite`
 * instead, so it works for both real files and the `stdout`/`stderr`
 * console pseudo-files the Rust side defines.
 */

typedef unsigned long size_t;
typedef __builtin_va_list va_list;
#define va_start(ap, last) __builtin_va_start(ap, last)
#define va_arg(ap, type) __builtin_va_arg(ap, type)
#define va_end(ap) __builtin_va_end(ap)
#define va_copy(dst, src) __builtin_va_copy(dst, src)

typedef struct FILE FILE; /* Opaque -- see cfile.rs. */

extern FILE *stdout;
extern FILE *stderr;
extern size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *file);
extern void konjac_write(const char *s, size_t len);

/* --- A handful of string.h basics printf itself needs ------------------- */

static size_t k_strlen(const char *s) {
    size_t n = 0;
    while (s[n] != '\0') {
        n++;
    }
    return n;
}

/* --- The actual formatter ------------------------------------------------
 *
 * Writes into `out` (a `cap`-byte buffer, or NULL/0 for a length-only
 * dry run -- how `snprintf(NULL, 0, ...)` is meant to behave), always
 * NUL-terminating if `cap > 0`, and returns the number of characters that
 * *would* have been written given unlimited space (matching real
 * vsnprintf's return value, so callers can detect truncation).
 */
static int format(char *out, size_t cap, const char *fmt, va_list args) {
    size_t written = 0; /* Characters actually placed into `out` so far. */
    size_t total = 0;   /* Characters that would exist with no cap. */

    /* Appends one character, respecting `cap`, and counts it either way. */
#define EMIT(ch)                                                                                                     \
    do {                                                                                                             \
        char c_ = (char)(ch);                                                                                       \
        if (out != 0 && written + 1 < cap) {                                                                         \
            out[written] = c_;                                                                                       \
            written++;                                                                                              \
        }                                                                                                            \
        total++;                                                                                                     \
    } while (0)

    for (const char *p = fmt; *p != '\0'; p++) {
        if (*p != '%') {
            EMIT(*p);
            continue;
        }
        p++;
        if (*p == '\0') {
            break;
        }

        int zero_pad = 0;
        int width = 0;
        if (*p == '0') {
            zero_pad = 1;
            p++;
        }
        while (*p >= '0' && *p <= '9') {
            width = width * 10 + (*p - '0');
            p++;
        }
        int precision = -1;
        if (*p == '.') {
            p++;
            precision = 0;
            while (*p >= '0' && *p <= '9') {
                precision = precision * 10 + (*p - '0');
                p++;
            }
        }
        int is_long = 0;
        while (*p == 'l') {
            is_long = 1;
            p++;
        }

        char conv = *p;
        if (conv == '%') {
            EMIT('%');
            continue;
        }
        if (conv == 'c') {
            EMIT((char)va_arg(args, int));
            continue;
        }
        if (conv == 's') {
            const char *s = va_arg(args, const char *);
            if (s == 0) {
                s = "(null)";
            }
            size_t len = k_strlen(s);
            if (precision >= 0 && (size_t)precision < len) {
                len = (size_t)precision;
            }
            size_t pad = (width > 0 && (size_t)width > len) ? (size_t)width - len : 0;
            while (pad-- > 0) {
                EMIT(' ');
            }
            for (size_t i = 0; i < len; i++) {
                EMIT(s[i]);
            }
            continue;
        }
        if (conv == 'd' || conv == 'i' || conv == 'u' || conv == 'x' || conv == 'X' || conv == 'o' || conv == 'p') {
            unsigned long uval;
            int negative = 0;
            int base = 10;
            const char *digits = "0123456789abcdef";
            if (conv == 'X') {
                digits = "0123456789ABCDEF";
            }
            if (conv == 'x' || conv == 'X') {
                base = 16;
            } else if (conv == 'o') {
                base = 8;
            }

            if (conv == 'p') {
                uval = (unsigned long)va_arg(args, void *);
                base = 16;
            } else if (conv == 'd' || conv == 'i') {
                long sval = is_long ? va_arg(args, long) : (long)va_arg(args, int);
                if (sval < 0) {
                    negative = 1;
                    uval = (unsigned long)(-sval);
                } else {
                    uval = (unsigned long)sval;
                }
            } else {
                uval = is_long ? va_arg(args, unsigned long) : (unsigned long)va_arg(args, unsigned int);
            }

            /* Render into a small local buffer backwards, then flush it
             * forwards -- simplest way to handle arbitrary width/base
             * without a second division pass just to count digits. */
            char digits_buf[32];
            int n = 0;
            /* Precision (`%.3d`) means "at least this many digits,
             * zero-filled" -- distinct from width (`%3d`, which pads
             * with spaces or, given a leading `0` flag, zeros) and NOT
             * applied when the value is 0 and precision is explicitly 0
             * (that case prints no digits at all, per the C standard --
             * not exercised by anything in this kernel today, but cheap
             * to get right while already in here). Doomgeneric's HUD
             * font lump names (`"STCFN%.3d"`) depend on exactly this to
             * come out zero-padded to 3 digits instead of bare numbers.
             */
            if (uval == 0 && !(precision == 0)) {
                digits_buf[n++] = '0';
            }
            while (uval != 0) {
                digits_buf[n++] = digits[uval % (unsigned)base];
                uval /= (unsigned)base;
            }
            while (precision >= 0 && n < precision) {
                digits_buf[n++] = '0';
            }
            if (conv == 'p') {
                digits_buf[n++] = 'x';
                digits_buf[n++] = '0';
            }

            int content_len = n + (negative ? 1 : 0);
            int pad = width - content_len;
            /* A precision was given: the digit buffer above already
             * carries all the zero-padding that's allowed, so the width
             * pass below must only ever use spaces here, never zeros
             * (matches every real printf: "0 flag ignored when a
             * precision is specified for a numeric conversion"). */
            if (precision >= 0) {
                zero_pad = 0;
            }
            if (negative && zero_pad && pad > 0) {
                EMIT('-');
                negative = 0; /* Sign already placed, before the zero padding. */
            } else if (negative) {
                EMIT('-');
            }
            while (pad-- > 0) {
                EMIT(zero_pad ? '0' : ' ');
            }
            while (n > 0) {
                EMIT(digits_buf[--n]);
            }
            continue;
        }

        /* Unknown conversion: print it back out literally rather than
         * silently eating an argument that was never actually consumed. */
        EMIT('%');
        EMIT(conv);
    }

    if (out != 0 && cap > 0) {
        out[written] = '\0';
    }
#undef EMIT
    return (int)total;
}

int vsnprintf(char *buf, size_t size, const char *fmt, va_list args) {
    return format(buf, size, fmt, args);
}

int snprintf(char *buf, size_t size, const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    int r = format(buf, size, fmt, args);
    va_end(args);
    return r;
}

int sprintf(char *buf, const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    /* No caller-given bound -- matches real (unsafe) sprintf semantics.
     * `(size_t)-1` is "unbounded" for `format`'s purposes. */
    int r = format(buf, (size_t)-1, fmt, args);
    va_end(args);
    return r;
}

int vprintf(const char *fmt, va_list args) {
    char buf[2048];
    int n = format(buf, sizeof(buf), fmt, args);
    size_t len = (size_t)n;
    if (len >= sizeof(buf)) {
        len = sizeof(buf) - 1; /* Truncated -- report what was actually written. */
    }
    konjac_write(buf, len);
    return n;
}

int printf(const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    int r = vprintf(fmt, args);
    va_end(args);
    return r;
}

int vfprintf(FILE *file, const char *fmt, va_list args) {
    char buf[2048];
    int n = format(buf, sizeof(buf), fmt, args);
    size_t len = (size_t)n;
    if (len >= sizeof(buf)) {
        len = sizeof(buf) - 1;
    }
    return (int)fwrite(buf, 1, len, file);
}

int fprintf(FILE *file, const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    int r = vfprintf(file, fmt, args);
    va_end(args);
    return r;
}

int puts(const char *s) {
    size_t len = k_strlen(s);
    konjac_write(s, len);
    konjac_write("\n", 1);
    return 0;
}

int putchar(int c) {
    char ch = (char)c;
    konjac_write(&ch, 1);
    return c;
}
