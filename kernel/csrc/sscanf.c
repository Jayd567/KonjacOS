// Minimal sscanf: only `%d`/`%i`/`%x`/`%u` (each optionally consuming a
// single leading integer argument, no field-width/`*`-skip support) are
// actually referenced anywhere in the doomgeneric core file set (grepped
// and confirmed -- m_config.c's config-file value parser is the only
// caller, using "%x" and "%i"), so this deliberately isn't a general
// sscanf: no %s/%c/%f, no whitespace-skipping between conversions beyond
// what leading-space skipping in each conversion already does. Extend
// this if a real compiler/linker error ever shows something else is
// needed -- see the top-level README's "driven by actual errors, not
// speculation" approach to this whole libc shim.

#include <stdarg.h>
#include <stdint.h>

static int is_space(char c)
{
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '\f';
}

static int is_digit(char c)
{
    return c >= '0' && c <= '9';
}

static int is_hex_digit(char c)
{
    return is_digit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
}

static int hex_value(char c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    return c - 'A' + 10;
}

int sscanf(const char *str, const char *fmt, ...)
{
    va_list ap;
    int matched = 0;
    const char *s = str;
    const char *f = fmt;

    va_start(ap, fmt);

    while (*f != '\0')
    {
        if (*f != '%')
        {
            if (is_space(*f))
            {
                while (is_space(*s)) { ++s; }
            }
            else if (*s == *f)
            {
                ++s;
            }
            else
            {
                break;
            }
            ++f;
            continue;
        }

        ++f; // skip '%'
        char conv = *f;
        ++f;

        while (is_space(*s)) { ++s; }

        if (conv == 'x' || conv == 'X')
        {
            if (!is_hex_digit(*s)) { break; }
            long value = 0;
            while (is_hex_digit(*s))
            {
                value = value * 16 + hex_value(*s);
                ++s;
            }
            *va_arg(ap, int *) = (int)value;
            ++matched;
        }
        else if (conv == 'd' || conv == 'i' || conv == 'u')
        {
            int neg = 0;
            if (*s == '-') { neg = 1; ++s; }
            else if (*s == '+') { ++s; }

            // "%i" additionally recognizes a "0x" prefix as hex, matching
            // real sscanf -- m_config.c relies on exactly this to accept
            // either decimal or "0x..." values through the same "%i".
            if (conv == 'i' && s[0] == '0' && (s[1] == 'x' || s[1] == 'X'))
            {
                s += 2;
                if (!is_hex_digit(*s)) { break; }
                long value = 0;
                while (is_hex_digit(*s))
                {
                    value = value * 16 + hex_value(*s);
                    ++s;
                }
                *va_arg(ap, int *) = (int)(neg ? -value : value);
                ++matched;
            }
            else
            {
                if (!is_digit(*s)) { break; }
                long value = 0;
                while (is_digit(*s))
                {
                    value = value * 10 + (*s - '0');
                    ++s;
                }
                *va_arg(ap, int *) = (int)(neg ? -value : value);
                ++matched;
            }
        }
        else
        {
            // Unsupported conversion -- stop rather than guess.
            break;
        }
    }

    va_end(ap);
    return matched;
}
