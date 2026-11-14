// SPDX-License-Identifier: MIT
#include "HooksLab.h"
#include "APInternal.h"
#include <dlfcn.h>
#include <stdio.h>
#include <stdatomic.h>
static int prepared;
static int (*own_original)(int);
static int (*shared_original)(const char *);
static int (*shared_entry)(const char *);
static _Atomic int shared_calls;
__attribute__((noinline)) static int own_entry(int value) {
    __asm__ volatile("nop\nnop\nnop\nnop\nnop\nnop\nnop\nnop");
    return value + 1;
}
static int own_replacement(int value) { return own_original(value) + 20; }
static int shared_replacement(const char *value) {
    atomic_fetch_add(&shared_calls, 1);
    return shared_original(value); // preserve behavior for all other callers
}
void lab_hooks_prepare(void) {
    if (prepared)
        return;
    prepared = 1;
    APFunctionHook((void *)own_entry, (void *)own_replacement, (void **)&own_original);
    shared_entry = (void *)dlsym(RTLD_DEFAULT, "atoi");
    if (shared_entry)
        APFunctionHook((void *)shared_entry, (void *)shared_replacement, (void **)&shared_original);
}
void lab_hooks_report(char *output, size_t capacity) {
    int own = own_entry(10);
    int before = atomic_load(&shared_calls);
    int shared = shared_entry ? shared_entry("41") : -1;
    int intercepted = atomic_load(&shared_calls) > before;
    snprintf(output, capacity,
             "{\"prepared\":%s,\"own\":%d,\"original\":%d,\"shared\":%d,\"sharedIntercepted\":%s,"
             "\"sharedOriginal\":%d,\"pending\":%llu,\"failed\":%llu,\"ready\":%llu}",
             prepared ? "true" : "false", own, own_original ? own_original(10) : -1, shared,
             intercepted ? "true" : "false", shared_original ? shared_original("41") : -1,
             (unsigned long long)__atomic_load_n(&APHookControl.pending, __ATOMIC_ACQUIRE),
             (unsigned long long)__atomic_load_n(&APHookControl.failed, __ATOMIC_ACQUIRE),
             (unsigned long long)__atomic_load_n(&APHookControl.ready, __ATOMIC_ACQUIRE));
}
