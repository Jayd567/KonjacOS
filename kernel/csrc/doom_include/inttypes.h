#ifndef KONJAC_INTTYPES_H
#define KONJAC_INTTYPES_H

/* doomtype.h only pulls this in for the C99 fixed-width integer types,
   never for the PRI-prefixed / SCN-prefixed format macros (grepped and
   confirmed across the whole included file set), so this just forwards
   to stdint.h. */
#include <stdint.h>

#endif
