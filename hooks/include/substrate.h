// SPDX-License-Identifier: MIT
// Binary interfaces used by existing Substrate/libhooker/blackjack tweaks.
#pragma once
#include "ApparitionHooks.h"
#include <stdbool.h>
#include <sys/types.h>
#ifdef __cplusplus
extern "C" {
#endif
extern bool MSDebug;
void *MSGetImageByName(const char *name);
void MSCloseImage(void *image);
void *MSFindSymbol(const void *image, const char *name);
void MSHookFunction(void *function, void *replacement, void **original);
void MSHookMessageEx(Class cls, SEL selector, IMP replacement, IMP *original);
void MSHookMemory(void *target, const void *bytes, size_t length);
void MSHookClassPair(Class target, Class hook, Class base);
void *MSHookIvar(id object, const char *name);
void MSHookProcess(pid_t pid, const char *library);
struct LHFunctionHookOptions {
    int options;
    int jmp_reg;
};
struct LHFunctionHook {
    void *function;
    void *replacement;
    void **oldptr;
    struct LHFunctionHookOptions *options;
};
struct LHMemoryPatch {
    void *destination;
    const void *data;
    size_t size;
    void *options;
};
const char *LHStrError(int code);
int LHHookFunctions(const struct LHFunctionHook *hooks, size_t count);
int LHPatchMemory(const struct LHMemoryPatch *patches, size_t count);
bool LHExecMemory(void **page, const void *data, size_t size);
void *LHOpenImage(const char *path);
void LHCloseImage(void *image);
bool LHFindSymbols(const void *image, const char *const *names, void **results, size_t count);
int LBHookMessage(Class cls, SEL selector, IMP replacement, IMP *original);
#ifdef __cplusplus
}
#endif
