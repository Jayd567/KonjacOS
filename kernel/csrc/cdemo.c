/* A tiny freestanding C demo: proves the whole "compile a real .c file with
 * a real C compiler, link it straight into the kernel binary, and have it
 * call back into Rust-implemented libc functions" pipeline works end to
 * end -- ahead of ever pulling in a real codebase like doomgeneric.
 *
 * No standard headers: this is `-ffreestanding` (see build.rs), so there's
 * no libc to include them from. Just the handful of declarations this file
 * actually needs, written out by hand.
 */

typedef unsigned long size_t;

/* Implemented in libc_shim.rs, routed onto the kernel's own heap
 * allocator. */
extern void *malloc(size_t size);
extern void free(void *ptr);

/* Implemented in commands.rs -- lets this C code report a result back to
 * Rust without needing to know anything about KonjacOS's console. */
extern void konjac_report(long value);

/* A deliberately non-trivial bit of C: allocate an array on the heap, fill
 * it, sum it, free it, report the sum back to Rust. If malloc/free are
 * wired up correctly (right size accounting, right alignment, actually
 * routing to real memory) this comes back with exactly the expected value;
 * if they're broken, this is exactly the kind of thing that corrupts
 * memory or crashes instead of quietly returning the wrong number.
 */
long cdemo_run(void) {
    int count = 64;
    int *nums = (int *)malloc((size_t)count * sizeof(int));
    if (nums == 0) {
        return -1;
    }

    long sum = 0;
    for (int i = 0; i < count; i++) {
        nums[i] = i * 2;
        sum += nums[i];
    }

    free(nums);
    konjac_report(sum);
    return sum;
}
