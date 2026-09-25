#ifndef KONJAC_UNISTD_H
#define KONJAC_UNISTD_H

/* Nothing in the included file set actually calls anything from real
   unistd.h (the isatty/fileno call in i_system.c is dead code, gated by
   #if ORIGCODE which is #undef'd) -- this header only needs to exist so
   the #include itself doesn't fail. */

#endif
