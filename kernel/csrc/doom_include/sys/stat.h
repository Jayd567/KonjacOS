#ifndef KONJAC_SYS_STAT_H
#define KONJAC_SYS_STAT_H

#include <sys/types.h>

/* Only mkdir() is actually referenced from the included file set
   (m_misc.c's M_MakeDirectory); nothing else in <sys/stat.h> is used, so
   this shim declares just that. */
int mkdir(const char *path, mode_t mode);

#endif
