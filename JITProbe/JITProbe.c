// The smallest JIT26 client: what an emulator does to run code it makes, and nothing else.
//
// 1. Ask the debugger for executable memory (brk #0xf00d, x16 = 1, x0 = 0: it allocates the
//    region and prepares every page, so TXM lets it run).
// 2. Map a writable alias of the same memory: code is written there and run from the
//    executable mapping.
// 3. Write `mov w0, #42; ret`, call it, then rewrite it to return 43 and call it again: a JIT
//    that can't replace code it already ran isn't one.
//
// Without a debugger the brk would kill the app, so it checks P_TRACED first.

#include "JITProbe.h"

#include <libkern/OSCacheControl.h>
#include <mach/mach.h>
#include <stdio.h>
#include <string.h>
#include <sys/sysctl.h>
#include <unistd.h>

static const size_t region_length = 64 * 1024;

__attribute__((naked, noinline)) static void *jit26_prepare_region(void *address, size_t length) {
    __asm__ volatile("mov x16, #1\n"
                     "brk #0xf00d\n"
                     "ret\n");
}

static int traced(void) {
    struct kinfo_proc info;
    size_t size = sizeof info;
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()};
    memset(&info, 0, sizeof info);
    if (sysctl(mib, 4, &info, &size, NULL, 0) != 0) return 0;
    return (info.kp_proc.p_flag & P_TRACED) != 0;
}

/// `mov w0, #value; ret`, written through `writable` and run from `executable`.
static int run_returning(void *writable, void *executable, uint16_t value) {
    uint32_t code[2] = {0x52800000u | ((uint32_t)value << 5), 0xd65f03c0u};
    memcpy(writable, code, sizeof code);
    sys_icache_invalidate(executable, sizeof code);
    return ((int (*)(void))executable)();
}

static void *executable;
static void *writable;

int jit_probe_run(char *report, size_t capacity) {
    if (!traced()) {
        snprintf(report, capacity, "No debugger attached: launch this app with JIT.");
        return 1;
    }
    if (!executable) {
        executable = jit26_prepare_region(NULL, region_length);
        if (!executable) {
            snprintf(report, capacity, "The debugger didn't give this app executable memory (x0 = 0).");
            return 2;
        }
        vm_address_t alias = 0;
        vm_prot_t current, maximum;
        kern_return_t remapped = vm_remap(mach_task_self(), &alias, region_length, 0, VM_FLAGS_ANYWHERE,
                                          mach_task_self(), (vm_address_t)executable, FALSE,
                                          &current, &maximum, VM_INHERIT_NONE);
        if (remapped != KERN_SUCCESS) {
            snprintf(report, capacity, "Got %p, but couldn't map it again (vm_remap: %d).", executable, remapped);
            executable = NULL;
            return 3;
        }
        kern_return_t protect = vm_protect(mach_task_self(), alias, region_length, FALSE, VM_PROT_READ | VM_PROT_WRITE);
        if (protect != KERN_SUCCESS) {
            snprintf(report, capacity, "Got %p, but couldn't make its alias writable (vm_protect: %d).", executable, protect);
            executable = NULL;
            return 4;
        }
        writable = (void *)alias;
    }
    int first = run_returning(writable, executable, 42);
    int second = run_returning(writable, executable, 43);
    int ok = first == 42 && second == 43;
    snprintf(report, capacity, "%s: code at %p (written through %p) returned %d, then %d after rewriting.",
             ok ? "JIT works" : "JIT ran but returned the wrong values", executable, writable, first, second);
    return ok ? 0 : 5;
}
