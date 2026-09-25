/* Minimal libm shim for the DOOM port: only sin/cos/tan/atan/fabs are
 * actually referenced anywhere in the doomgeneric core file set (grepped
 * and confirmed -- see README), and only during one-time startup table
 * generation in r_main.c, not per-frame, so there's no need for a real
 * libm here. These use the x87 FPU's own transcendental instructions
 * directly via inline asm, which is safe under KonjacOS: every task
 * already owns a private FXSAVE/FXRSTOR area (covers x87 state, not just
 * SSE) from the multitasking work, so nothing here can clobber another
 * task's FPU state across a context switch.
 */

double sin(double x)
{
    double result;
    __asm__ volatile("fsin" : "=t"(result) : "0"(x));
    return result;
}

double cos(double x)
{
    double result;
    __asm__ volatile("fcos" : "=t"(result) : "0"(x));
    return result;
}

double tan(double x)
{
    /* No single x87 instruction gives plain tan(x) directly in a form
     * that's simple to bind through GCC/clang inline asm constraints, so
     * derive it from sin/cos -- fine given this is only ever called
     * during one-time table generation, not a hot path. */
    return sin(x) / cos(x);
}

double atan(double x)
{
    /* fpatan computes atan2(y, x) = atan(y/x) from ST(1)/ST(0); passing
     * 1.0 as the "x" operand collapses it to plain atan(x). */
    double result;
    __asm__ volatile(
        "fld1\n\t"
        "fpatan"
        : "=t"(result) : "0"(x));
    return result;
}

double fabs(double x)
{
    return __builtin_fabs(x);
}
