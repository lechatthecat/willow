// Link-only fault injection: no test hooks or environment branches in the runtime.
#include <stdint.h>
#include <stddef.h>

static _Thread_local int saw_bar;
extern void __real_willow_println_i64(int64_t);
extern int32_t __real_willow_sync_safepoint(void);
extern void __real_willow_pop_roots(int32_t);

void __wrap_willow_println_i64(int64_t value) {
    __real_willow_println_i64(value);
    // bar's observable output arms the injection independently of the
    // compiler's diagnostic publication mechanism.
    if (value == 7) { saw_bar = 1; }
}

__attribute__((noinline)) static unsigned burn_stack(unsigned depth) {
    volatile unsigned char page[8192];
    // Probe each page rather than jumping past the protected task guard.
    page[0] = 1;
    page[4096] = 2;
    page[8191] = 3;
    if (depth == 0) { return page[0]; }
    return burn_stack(depth - 1) + page[0] + page[4096] + page[8191];
}

int32_t __wrap_willow_sync_safepoint(void) {
    // bar has no loop/synchronous safepoint. The first armed safepoint is
    // foo's loop after bar returned, even if bar's marker was never restored.
#ifndef OVERFLOW_CLEANUP
    if (saw_bar) { return (int32_t)burn_stack(1000000); }
#endif
    return __real_willow_sync_safepoint();
}

void __wrap_willow_pop_roots(int32_t count) {
#ifdef OVERFLOW_CLEANUP
    // bar has no GC roots; foo must pop its String root after bar returns.
    if (saw_bar) { (void)burn_stack(1000000); }
#endif
    __real_willow_pop_roots(count);
}
