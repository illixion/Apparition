// SPDX-License-Identifier: MIT
#pragma once
#include <stddef.h>
#include <objc/runtime.h>

#ifdef __cplusplus
extern "C" {
#endif

// Low-level data-only hooks, also used by the compatibility API in substrate.h.
// These calls do not allocate or modify executable memory.
typedef struct {
    size_t rebound;
    size_t unavailable;
    size_t protection_failures;
} APHookResult;

// Rebind unauthenticated import slots in loaded images, including classic lazy
// imports. This cannot intercept direct calls or authenticated slots.
// Call again after loading additional images. Registration is not persistent.
// The caller must keep replacement loaded for the lifetime of those images.
// The caller must exclude concurrent image unloading. A protection_failures
// result means a slot changed but its page protection could not be restored.
APHookResult APHookImports(void *target, void *replacement);

// Returns false for a missing method; inherited methods are overridden only on
// the requested class. Publish the original before installing the replacement.
int APHookMessage(Class cls, SEL selector, IMP replacement, IMP *original);

// dlsym lookup, accepting either a C name or Mach-O's leading underscore.
void *APFindSymbol(void *image, const char *name);

#ifdef __cplusplus
}
#endif
