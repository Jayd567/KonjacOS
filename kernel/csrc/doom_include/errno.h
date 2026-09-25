#ifndef KONJAC_ERRNO_H
#define KONJAC_ERRNO_H

/* No real per-task errno variable yet -- a single global is fine since
   KonjacOS's C code here is not (yet) reentrant/threaded in a way that
   would race on it, and doomgeneric's core files only ever check it
   right after a call that might have set it. */
extern int errno;

#define EISDIR   21
#define ENOENT    2
#define EACCES   13
#define EEXIST   17
#define EINVAL   22
#define ENOSYS   38

#endif
