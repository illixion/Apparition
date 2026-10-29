// Can code the debugger patched into the app's own signed __TEXT still run once it detaches?
//
// If it can, a debugger could rewrite a JIT's store sites at launch (branching them to stubs that
// write through an RW alias) instead of trapping every store at run time. Each target sits on its
// own 16 KiB page, so one page's state can't vouch for another's:
//   a: run before the patch, patched while attached, run while attached and after detach
//   b: patched while attached, first run only after detach
//   c: never patched (control)
//   d: patched into a branch to an unpatched trampoline (the store-site rewrite in miniature)
// Each returns 1 unpatched; a and b return 2 patched, d returns 3.

#include "TextPatch.h"

#include <fcntl.h>
#include <libkern/OSCacheControl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/proc.h>
#include <sys/sysctl.h>
#include <time.h>
#include <unistd.h>

int lab_target_a(void);
int lab_target_b(void);
int lab_target_c(void);
int lab_target_d(void);
int lab_trampoline(void);

__asm__(
    ".text\n"
    ".p2align 14\n"
    ".globl _lab_target_a\n"
    "_lab_target_a:\n"
    "    mov w0, #1\n"
    "    ret\n"
    ".p2align 14\n"
    ".globl _lab_target_b\n"
    "_lab_target_b:\n"
    "    mov w0, #1\n"
    "    ret\n"
    ".p2align 14\n"
    ".globl _lab_target_c\n"
    "_lab_target_c:\n"
    "    mov w0, #1\n"
    "    ret\n"
    ".p2align 14\n"
    ".globl _lab_target_d\n"
    "_lab_target_d:\n"
    "    mov w0, #1\n"
    "    ret\n"
    ".globl _lab_trampoline\n"
    "_lab_trampoline:\n"
    "    mov w0, #3\n"
    "    ret\n"
    ".p2align 14\n");

// Apparition's experimental JIT26 command: copy len bytes from src to dst. Returns 0 on success.
__attribute__((naked, noinline)) static uint64_t jit26_write(void *dst, const void *src, uint64_t len) {
    __asm__ volatile("mov x16, #0x4150\n"
                     "brk #0xf00d\n"
                     "ret\n");
}

__attribute__((naked, noinline)) static void jit26_detach(void) {
    __asm__ volatile("mov x16, #0\n"
                     "brk #0xf00d\n"
                     "ret\n");
}

extern int csops(pid_t pid, unsigned int ops, void *useraddr, size_t usersize);

static uint32_t cs_flags(void) {
    uint32_t flags = 0;
    csops(getpid(), 0 /* CS_OPS_STATUS */, &flags, sizeof flags);
    return flags;
}

static int traced(void) {
    struct kinfo_proc info;
    size_t size = sizeof info;
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()};
    memset(&info, 0, sizeof info);
    if (sysctl(mib, 4, &info, &size, NULL, 0) != 0) return -1;
    return (info.kp_proc.p_flag & P_TRACED) != 0;
}

static int crumbs = -1;

static void step(const char *format, ...) {
    if (crumbs < 0) return;
    char line[256];
    va_list args;
    va_start(args, format);
    int length = vsnprintf(line, sizeof line - 1, format, args);
    va_end(args);
    if (length < 0) return;
    if (length > (int)sizeof line - 2) length = sizeof line - 2;
    line[length++] = '\n';
    write(crumbs, line, (size_t)length);
    fsync(crumbs);
}

static uint64_t patch(void *site, uint32_t instruction) {
    uint64_t result = jit26_write(site, &instruction, sizeof instruction);
    sys_icache_invalidate(site, sizeof instruction);
    return result;
}

void text_patch_run(const char *breadcrumbs, char *report, size_t capacity) {
    crumbs = open(breadcrumbs, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    int was_traced = traced();
    uint32_t flags_before = cs_flags();
    step("start traced=%d csflags=%#x a=%p b=%p c=%p d=%p", was_traced, flags_before,
         (void *)lab_target_a, (void *)lab_target_b, (void *)lab_target_c, (void *)lab_target_d);

    if (was_traced != 1) {
        step("not traced: control run only");
        int a = lab_target_a(), b = lab_target_b(), c = lab_target_c(), d = lab_target_d();
        snprintf(report, capacity,
                 "{\"traced\":false,\"csFlags\":\"%#x\",\"a\":%d,\"b\":%d,\"c\":%d,\"d\":%d}",
                 flags_before, a, b, c, d);
        step("done %s", report);
        close(crumbs);
        return;
    }

    step("a before patch");
    int a_before = lab_target_a();

    // mov w0, #2 for a and b; b <lab_trampoline> for d.
    uint32_t mov2 = 0x52800040;
    intptr_t offset = ((intptr_t)lab_trampoline - (intptr_t)lab_target_d) / 4;
    uint32_t branch = 0x14000000 | ((uint32_t)offset & 0x03ffffff);
    step("patching");
    uint64_t wrote_a = patch((void *)lab_target_a, mov2);
    uint64_t wrote_b = patch((void *)lab_target_b, mov2);
    uint64_t wrote_d = patch((void *)lab_target_d, branch);
    step("patched a=%llu b=%llu d=%llu (0 = ok)", wrote_a, wrote_b, wrote_d);

    step("a while attached");
    int a_attached = lab_target_a();
    step("a while attached = %d", a_attached);

    step("detaching");
    jit26_detach();
    usleep(500 * 1000);
    int after_traced = traced();
    uint32_t flags_after = cs_flags();
    step("detached traced=%d csflags=%#x", after_traced, flags_after);

    step("a after detach");
    int a_after = lab_target_a();
    step("a after detach = %d; b first run after detach", a_after);
    int b_after = lab_target_b();
    step("b = %d; c control", b_after);
    int c_after = lab_target_c();
    step("c = %d; d trampoline", c_after);
    int d_after = lab_target_d();
    step("d = %d", d_after);

    snprintf(report, capacity,
             "{\"traced\":true,\"csFlagsBefore\":\"%#x\",\"csFlagsAfter\":\"%#x\",\"tracedAfterDetach\":%d,"
             "\"writes\":[%llu,%llu,%llu],\"aBefore\":%d,\"aAttached\":%d,\"aAfter\":%d,"
             "\"bAfter\":%d,\"cAfter\":%d,\"dAfter\":%d}",
             flags_before, flags_after, after_traced, wrote_a, wrote_b, wrote_d, a_before, a_attached,
             a_after, b_after, c_after, d_after);
    step("done %s", report);
    close(crumbs);
}

void text_patch_recheck(char *report, size_t capacity) {
    snprintf(report, capacity, "{\"traced\":%d,\"a\":%d,\"b\":%d,\"c\":%d,\"d\":%d}", traced(), lab_target_a(),
             lab_target_b(), lab_target_c(), lab_target_d());
}

extern uint64_t lab_optimized_target(uint64_t);
extern uint64_t lab_optimized_inlined_caller(uint64_t);
extern uint64_t lab_optimized_replacement(uint64_t);

// Report raw samples, not a performance verdict: thermal state, scheduling and
// the tiny function's branch prediction can dominate these measurements.
static double optimized_sample(uint64_t *checksum) {
    struct timespec start, end;
    uint64_t sum = 0;
    clock_gettime(CLOCK_MONOTONIC_RAW, &start);
    for (uint64_t i = 0; i < 1000000; ++i) sum += lab_optimized_target(i);
    clock_gettime(CLOCK_MONOTONIC_RAW, &end);
    *checksum = sum;
    return ((end.tv_sec - start.tv_sec) * 1e9 + end.tv_nsec - start.tv_nsec) / 1000000.0;
}

void optimized_patch_run(const char *breadcrumbs, char *report, size_t capacity) {
    crumbs = open(breadcrumbs, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (traced() != 1) {
        snprintf(report, capacity, "{\"status\":\"not-traced\",\"direct\":%llu,\"inlined\":%llu}",
                 lab_optimized_target(10), lab_optimized_inlined_caller(10));
        step("%s", report);
        if (crumbs >= 0) close(crumbs);
        crumbs = -1;
        return;
    }
    uintptr_t site = (uintptr_t)lab_optimized_target;
    uintptr_t destination = (uintptr_t)lab_optimized_replacement;
    int64_t distance = (int64_t)destination - (int64_t)site;
    // ARM64 B has a signed 26-bit immediate in instruction units. Refuse rather
    // than truncate an out-of-range branch or overwrite an unaligned entry.
    if ((site & 3) || (destination & 3) || distance < -134217728 || distance >= 134217728) {
        snprintf(report, capacity, "{\"status\":\"branch-out-of-range\"}");
        step("%s", report);
        if (crumbs >= 0) close(crumbs);
        crumbs = -1;
        return;
    }
    uint32_t original;
    memcpy(&original, (void *)site, sizeof original);
    uint32_t branch = 0x14000000 | ((uint32_t)(distance / 4) & 0x03ffffff);
    double timings[3][5];
    uint64_t sums[3] = {0}, direct[3] = {0}, inlined[3] = {0};
    uint64_t writes[2] = {1, 1};
    step("optimized start entry=%p replacement=%p original=%#x branch=%#x", (void *)site,
         (void *)destination, original, branch);
    for (int phase = 0; phase < 3; ++phase) {
        if (phase > 0) {
            writes[phase - 1] = patch((void *)site, phase == 1 ? branch : original);
            step("phase=%d write=%llu", phase, writes[phase - 1]);
        }
        direct[phase] = lab_optimized_target(10);
        inlined[phase] = lab_optimized_inlined_caller(10);
        // Warm the entry after rewriting it before collecting samples.
        optimized_sample(&sums[phase]);
        for (int sample = 0; sample < 5; ++sample)
            timings[phase][sample] = optimized_sample(&sums[phase]);
        step("phase=%d direct=%llu inlined=%llu checksum=%llu", phase, direct[phase],
             inlined[phase], sums[phase]);
    }
    int passed = writes[0] == 0 && writes[1] == 0 && direct[0] == 11 && direct[1] == 17 &&
        direct[2] == 11 && inlined[0] == 11 && inlined[1] == 11 && inlined[2] == 11 &&
        sums[0] == 500000500000ULL && sums[1] == 500006500000ULL && sums[2] == sums[0];
    snprintf(report, capacity,
             "{\"status\":\"%s\",\"iterations\":1000000,\"writes\":[%llu,%llu],"
             "\"direct\":[%llu,%llu,%llu],\"inlined\":[%llu,%llu,%llu],"
             "\"checksum\":[%llu,%llu,%llu],\"nsPerCall\":[[%.3f,%.3f,%.3f,%.3f,%.3f],"
             "[%.3f,%.3f,%.3f,%.3f,%.3f],[%.3f,%.3f,%.3f,%.3f,%.3f]]}",
             passed ? "passed" : "failed", writes[0], writes[1], direct[0], direct[1], direct[2],
             inlined[0], inlined[1], inlined[2], sums[0], sums[1], sums[2],
             timings[0][0], timings[0][1], timings[0][2], timings[0][3], timings[0][4],
             timings[1][0], timings[1][1], timings[1][2], timings[1][3], timings[1][4],
             timings[2][0], timings[2][1], timings[2][2], timings[2][3], timings[2][4]);
    step("%s", report);
    if (crumbs >= 0) close(crumbs);
    crumbs = -1;
}
