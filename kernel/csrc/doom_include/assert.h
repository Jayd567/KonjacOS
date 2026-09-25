#ifndef KONJAC_ASSERT_H
#define KONJAC_ASSERT_H

/* No-op assert: DOOM's core files barely use this, and a kernel-side
   panic-on-assert would need konjac_panic wired in here. Keeping this a
   no-op is safe and matches how NDEBUG builds behave everywhere else. */
#define assert(expr) ((void)0)

#endif
