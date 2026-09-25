/* string.h functions that aren't already provided by intrinsics.rs
 * (memcpy/memset/memmove/memcmp/strlen live there, backing the compiler's
 * own implicit calls as well as C code -- see that file's doc comment).
 * Everything here is a completely ordinary, textbook implementation;
 * there's nothing KonjacOS-specific about any of it.
 */

#include <string.h>
#include <stdlib.h>

char *strcpy(char *dest, const char *src)
{
    char *d = dest;
    while ((*d++ = *src++) != '\0') {}
    return dest;
}

char *strncpy(char *dest, const char *src, size_t n)
{
    size_t i;
    for (i = 0; i < n && src[i] != '\0'; ++i)
    {
        dest[i] = src[i];
    }
    for (; i < n; ++i)
    {
        dest[i] = '\0';
    }
    return dest;
}

char *strcat(char *dest, const char *src)
{
    char *d = dest;
    while (*d != '\0') { ++d; }
    while ((*d++ = *src++) != '\0') {}
    return dest;
}

char *strncat(char *dest, const char *src, size_t n)
{
    char *d = dest;
    size_t i;
    while (*d != '\0') { ++d; }
    for (i = 0; i < n && src[i] != '\0'; ++i)
    {
        d[i] = src[i];
    }
    d[i] = '\0';
    return dest;
}

int strcmp(const char *a, const char *b)
{
    while (*a != '\0' && *a == *b)
    {
        ++a;
        ++b;
    }
    return (unsigned char)*a - (unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n)
{
    size_t i;
    for (i = 0; i < n; ++i)
    {
        unsigned char ca = (unsigned char)a[i];
        unsigned char cb = (unsigned char)b[i];
        if (ca != cb || ca == '\0')
        {
            return ca - cb;
        }
    }
    return 0;
}

static int ascii_tolower(int c)
{
    if (c >= 'A' && c <= 'Z')
    {
        return c - 'A' + 'a';
    }
    return c;
}

int strcasecmp(const char *a, const char *b)
{
    while (*a != '\0' && ascii_tolower((unsigned char)*a) == ascii_tolower((unsigned char)*b))
    {
        ++a;
        ++b;
    }
    return ascii_tolower((unsigned char)*a) - ascii_tolower((unsigned char)*b);
}

int strncasecmp(const char *a, const char *b, size_t n)
{
    size_t i;
    for (i = 0; i < n; ++i)
    {
        int ca = ascii_tolower((unsigned char)a[i]);
        int cb = ascii_tolower((unsigned char)b[i]);
        if (ca != cb || ca == '\0')
        {
            return ca - cb;
        }
    }
    return 0;
}

char *strchr(const char *s, int c)
{
    while (*s != '\0')
    {
        if (*s == (char)c)
        {
            return (char *)s;
        }
        ++s;
    }
    return (c == '\0') ? (char *)s : NULL;
}

char *strrchr(const char *s, int c)
{
    const char *last = NULL;
    while (*s != '\0')
    {
        if (*s == (char)c)
        {
            last = s;
        }
        ++s;
    }
    if (c == '\0')
    {
        return (char *)s;
    }
    return (char *)last;
}

char *strstr(const char *haystack, const char *needle)
{
    size_t needle_len = strlen(needle);
    if (needle_len == 0)
    {
        return (char *)haystack;
    }
    for (; *haystack != '\0'; ++haystack)
    {
        if (strncmp(haystack, needle, needle_len) == 0)
        {
            return (char *)haystack;
        }
    }
    return NULL;
}

char *strdup(const char *s)
{
    size_t len = strlen(s) + 1;
    char *copy = malloc(len);
    if (copy != NULL)
    {
        memcpy(copy, s, len);
    }
    return copy;
}

char *strtok(char *str, const char *delim)
{
    static char *saved;
    char *start;

    if (str != NULL)
    {
        saved = str;
    }
    if (saved == NULL)
    {
        return NULL;
    }

    while (*saved != '\0' && strchr(delim, *saved) != NULL)
    {
        ++saved;
    }
    if (*saved == '\0')
    {
        saved = NULL;
        return NULL;
    }

    start = saved;
    while (*saved != '\0' && strchr(delim, *saved) == NULL)
    {
        ++saved;
    }
    if (*saved != '\0')
    {
        *saved = '\0';
        ++saved;
    }
    else
    {
        saved = NULL;
    }
    return start;
}

char *strerror(int errnum)
{
    (void)errnum;
    return "error";
}
