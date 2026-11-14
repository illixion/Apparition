// SPDX-License-Identifier: MIT
#include "APInternal.h"
#include "substrate.h"
#include <stdlib.h>
#include <errno.h>
#include <dlfcn.h>
#include <unistd.h>

bool MSDebug = false;
void *MSGetImageByName(const char *name) { return APOpenImage(name); }
void MSCloseImage(void *image) { (void)image; }
void *MSFindSymbol(const void *image, const char *name) { return APImageSymbol(image, name); }
void MSHookFunction(void *f, void *r, void **o) { (void)APFunctionHook(f, r, o); }
void MSHookMessageEx(Class c, SEL s, IMP r, IMP *o) { (void)APHookMessage(c, s, r, o); }
void MSHookMemory(void *t, const void *b, size_t n) { (void)APPatchMemory(t, b, n, true); }
void *MSHookIvar(id object, const char *name) {
    if (!object || !name)
        return NULL;
    Ivar ivar = class_getInstanceVariable(object_getClass(object), name);
    return ivar ? (char *)(void *)object + ivar_getOffset(ivar) : NULL;
}
void MSHookProcess(pid_t pid, const char *library) {
    // Process injection is outside a jailed library's authority. Loading into
    // this process is supported; another process is refused deterministically.
    if (pid != getpid() || !library) {
        errno = ENOTSUP;
        return;
    }
    if (!dlopen(library, RTLD_NOW | RTLD_GLOBAL))
        errno = ENOENT;
}
static void pair_methods(Class target, Class hook, Class base) {
    unsigned count = 0;
    Method *methods = class_copyMethodList(hook, &count);
    for (unsigned i = 0; i < count; ++i) {
        SEL selector = method_getName(methods[i]);
        Method old = class_getInstanceMethod(target, selector);
        if (old && base)
            class_replaceMethod(base, selector, method_getImplementation(old),
                                method_getTypeEncoding(old));
        class_replaceMethod(target, selector, method_getImplementation(methods[i]),
                            method_getTypeEncoding(methods[i]));
    }
    free(methods);
}
void MSHookClassPair(Class target, Class hook, Class base) {
    if (!target || !hook || target == hook)
        return;
    pair_methods(target, hook, base);
    pair_methods(object_getClass((id)target), object_getClass((id)hook),
                 base ? object_getClass((id)base) : Nil);
}
const char *LHStrError(int code) {
    static const char *const errors[] = {"Success",
                                         "Selector not found",
                                         "Function is too short",
                                         "Unsupported instruction at entry",
                                         "Memory unavailable or Apparition JIT required",
                                         "Symbol not found"};
    return code >= 0 && code < 6 ? errors[code] : "Unknown hook error";
}
int LHHookFunctions(const struct LHFunctionHook *hooks, size_t count) {
    if ((!hooks && count) || count > AP_MAX_HOOKS)
        return 5;
    for (size_t i = 0; i < count; ++i) {
        // The engine's entry shape uses x16. Never silently accept a different
        // caller-selected scratch register.
        if (hooks[i].options &&
            (hooks[i].options->options != 0 &&
             (hooks[i].options->options != 1 || hooks[i].options->jmp_reg != 16)))
            return 3;
        int result = APFunctionHook(hooks[i].function, hooks[i].replacement, hooks[i].oldptr);
        if (result)
            return result;
    }
    return 0;
}
int LHPatchMemory(const struct LHMemoryPatch *patches, size_t count) {
    if ((!patches && count) || count > AP_MAX_HOOKS)
        return 4;
    for (size_t i = 0; i < count; ++i) {
        if (patches[i].options)
            return 4;
        int result = APPatchMemory(patches[i].destination, patches[i].data, patches[i].size, false);
        if (result)
            return result;
    }
    return 0;
}
bool LHExecMemory(void **page, const void *data, size_t size) {
    if (!page)
        return false;
    *page = APExecMemory(data, size);
    return *page != NULL;
}
void *LHOpenImage(const char *path) { return APOpenImage(path); }
void LHCloseImage(void *image) { (void)image; }
bool LHFindSymbols(const void *image, const char *const *names, void **results, size_t count) {
    if ((!names || !results) && count)
        return false;
    bool found = true;
    for (size_t i = 0; i < count; ++i) {
        results[i] = APImageSymbol(image, names[i]);
        if (!results[i])
            found = false;
    }
    return found;
}
int LBHookMessage(Class c, SEL s, IMP r, IMP *o) { return APHookMessage(c, s, r, o) ? 0 : 1; }
