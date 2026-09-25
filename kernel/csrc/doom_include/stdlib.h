#ifndef KONJAC_STDLIB_H
#define KONJAC_STDLIB_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

void *malloc(size_t size);
void *calloc(size_t nmemb, size_t size);
void *realloc(void *ptr, size_t size);
void free(void *ptr);

int atoi(const char *s);
double atof(const char *s);
long strtol(const char *s, char **endptr, int base);
double strtod(const char *s, char **endptr);

int abs(int n);
long labs(long n);

void exit(int status) __attribute__((noreturn));
void abort(void) __attribute__((noreturn));

char *getenv(const char *name);
int system(const char *command);

void qsort(void *base, size_t nmemb, size_t size,
           int (*compar)(const void *, const void *));
int rand(void);
void srand(unsigned int seed);

#ifdef __cplusplus
}
#endif

#endif
